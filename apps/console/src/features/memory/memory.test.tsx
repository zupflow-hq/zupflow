import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  type Agent,
  type ApiKey,
  createClient,
  createHttpTransport,
  type DefaultMemoryType,
  IDEMPOTENCY_HEADER,
  MEMORY_KIND_NOTES,
  type Member,
  type Memory,
  type MemoryKind,
  type MemoryOrigin,
  type MemoryType,
  type Organization,
  type User,
  type Workspace,
  ZupflowApiError,
  ZupflowNetworkError,
} from "@zupflow/sdk";
import { toast } from "sonner";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Form } from "@/components/ui/form";
import { TooltipProvider } from "@/components/ui/tooltip";
import { AgentFields, agentInput, useAgentForm } from "@/features/agents/agent-form";
import { shortId } from "@/lib/format";
import { newId } from "@/mocks/ids";
import { Route } from "@/routes/_authed/$org/memory";
import { AgentMemories, PRODUCTION_MEMORIES } from "./agent-memories";

const { api, scope } = vi.hoisted(() => ({
  api: {
    memoryKinds: { list: vi.fn() },
    memoryTypes: {
      list: vi.fn(),
      get: vi.fn(),
      getDefault: vi.fn(),
      create: vi.fn(),
      update: vi.fn(),
      delete: vi.fn(),
      setDefault: vi.fn(),
    },
    memories: { list: vi.fn(), create: vi.fn(), update: vi.fn(), delete: vi.fn(), deleteAll: vi.fn() },
    members: { list: vi.fn() },
    apiKeys: { list: vi.fn() },
    models: { list: vi.fn(), get: vi.fn(), getDefault: vi.fn() },
  },
  scope: {} as { org: Organization },
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));
vi.mock("@/features/context", async (original) => ({
  ...(await original<typeof import("@/features/context")>()),
  useOrg: () => scope.org,
  useDocumentTitle: vi.fn(),
}));
vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  Link: ({ children, to, params }: { children: React.ReactNode; to: string; params?: Record<string, string> }) => (
    <a href={Object.entries(params ?? {}).reduce((path, [key, value]) => path.replace(`$${key}`, value), to)}>
      {children}
    </a>
  ),
}));

const at = "2026-10-20T12:00:00.000Z";
const later = "2026-10-21T09:30:00.000Z";
const org: Organization = {
  id: newId("organization"),
  name: "Fernhill",
  slug: "fernhill",
  role: "owner",
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
const as = (role: Organization["role"]) => ({ ...org, role });

const jordan: User = { id: newId("user"), name: "Jordan Lee", email: "jordan@fernhill.example", avatar_url: null };
const member: Member = {
  id: newId("member"),
  organization_id: org.id,
  user: jordan,
  role: "owner",
  status: "active",
  invited_at: at,
  expires_at: null,
  joined_at: at,
  last_active_at: at,
};
const key: ApiKey = {
  id: newId("api_key"),
  organization_id: org.id,
  environment: "sandbox",
  name: "Queue worker",
  prefix: "zf_test_A1b2",
  last4: "7890",
  created_by: jordan,
  created_at: at,
  last_used_at: null,
  revoked_at: null,
};

const notesKind: MemoryKind = {
  kind: MEMORY_KIND_NOTES,
  name: "Notes",
  description: "Short notes the agent saves with its tools, or people add.",
  settings_schema: {
    type: "object",
    properties: {
      recall_characters: {
        type: "integer",
        minimum: 0,
        maximum: 32_000,
        default: 8000,
        title: "Recall length",
        description: "How many characters of notes a run starts with.",
      },
      max_notes: {
        type: "integer",
        minimum: 1,
        maximum: 1000,
        default: 200,
        title: "Note limit",
        description: "The most notes one agent keeps in one workspace.",
      },
    },
  },
  manage: { list: true, add: true, change: true, forget: true },
};
const memoryType = (name: string, fields: Partial<MemoryType> = {}): MemoryType => ({
  id: newId("memory_type"),
  name,
  kind: MEMORY_KIND_NOTES,
  settings: { max_notes: 200, recall_characters: 8000 },
  writes: "allow",
  served: true,
  agent_count: 0,
  is_default: false,
  created_at: at,
  updated_at: at,
  ...fields,
});
const notes = memoryType("Notes", { is_default: true });
const reviewed = memoryType("Reviewed notes", {
  settings: { max_notes: 50, recall_characters: 4000 },
  writes: "deny",
  agent_count: 1,
});
const unserved = memoryType("Support history", {
  kind: "archive",
  settings: { dimensions: 1536 },
  served: false,
});
const defaultIs = (type: MemoryType | null): DefaultMemoryType => ({
  memory_type: type,
  updated_at: type ? at : null,
  updated_by: type ? jordan : null,
});

const sandbox: Workspace = {
  id: newId("workspace"),
  organization_id: org.id,
  name: "Acme",
  slug: "acme",
  environment: "sandbox",
  external_id: null,
  created_at: at,
  stats: { agents: 1, installations: 0, runs_last_24h: 0 },
};
const production: Workspace = { ...sandbox, id: newId("workspace"), slug: "acme-live", environment: "production" };
const agent: Agent = {
  id: newId("agent"),
  workspace_id: sandbox.id,
  name: "Publishing assistant",
  description: "",
  model_id: null,
  memory: "default",
  memory_type_id: null,
  instructions: "",
  status: "active",
  grant_count: 0,
  runs_last_24h: 0,
  last_run_at: null,
  created_at: at,
  updated_at: at,
};

const runId = newId("run");
const fromRun: MemoryOrigin = { type: "run", run_id: runId, tool_call_id: newId("tool_call"), after_app_content: true };
const memory = (short_id: string, text: string, origin: MemoryOrigin, fields: Partial<Memory> = {}): Memory => ({
  id: newId("memory"),
  workspace_id: sandbox.id,
  agent_id: agent.id,
  memory_type_id: notes.id,
  short_id,
  text,
  channel_conversation_id: null,
  origin,
  created_at: at,
  updated_at: at,
  ...fields,
});
const britishSpelling = memory("k7m2qd", "The blog's style guide asks for British spelling.", fromRun, {
  updated_at: later,
  channel_conversation_id: newId("channel_conversation"),
});
const shortPosts = memory("tone2a", "Keep posts under 300 words.", { type: "api_key", api_key_id: key.id });
const drafts = memory("draft3", "Send drafts as a shared document link.", { type: "member", user_id: jordan.id });

const page = <T,>(items: T[], next_cursor: string | null = null, total = items.length) => ({
  items,
  pagination: { next_cursor, total },
});

/** What the server answers for reads, unless a test says otherwise. */
function serve({
  types = [notes, reviewed],
  current = notes,
  memories = [britishSpelling, shortPosts, drafts],
}: {
  types?: MemoryType[];
  current?: MemoryType | null;
  memories?: Memory[];
} = {}) {
  api.memoryKinds.list.mockResolvedValue([notesKind]);
  api.memoryTypes.list.mockImplementation(async (_org, query: { limit: number }) =>
    page(types.slice(0, query.limit), types.length > query.limit ? "next" : null, types.length),
  );
  api.memoryTypes.get.mockImplementation(async (_org, id: string) => {
    const found = types.find((t) => t.id === id);
    if (!found) throw new ZupflowApiError(404, { code: "not_found", message: "No such memory type.", fields: null });
    return found;
  });
  api.memoryTypes.getDefault.mockResolvedValue(defaultIs(current));
  api.memories.list.mockResolvedValue(page(memories));
  api.members.list.mockResolvedValue([member]);
  api.apiKeys.list.mockResolvedValue([key]);
  api.models.list.mockResolvedValue(page([]));
  api.models.getDefault.mockResolvedValue({ environment: "sandbox", model: null, updated_at: null, updated_by: null });
}

const clients: QueryClient[] = [];
/** A client for the tests; `live`, its answers go stale at once, as the Console's do between visits. */
function client({ live = false } = {}) {
  const c = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: live ? 0 : Infinity }, mutations: { retry: false } },
  });
  clients.push(c);
  return c;
}
function renderWithClient(ui: React.ReactNode, options: { live?: boolean } = {}) {
  const c = client(options);
  const wrap = (node: React.ReactNode) => (
    <QueryClientProvider client={c}>
      <TooltipProvider>{node}</TooltipProvider>
    </QueryClientProvider>
  );
  const view = render(wrap(ui));
  return { ...view, client: c, change: (next: React.ReactNode) => view.rerender(wrap(next)) };
}

/** A promise a test settles when it chooses, for an answer still on its way. */
function deferred<T>() {
  let settle: (value: T) => void = () => {};
  const promise = new Promise<T>((resolve) => {
    settle = resolve;
  });
  return { promise, settle };
}

/** The memory types table's row for a type, by its name. */
async function rowOf(name: string) {
  const rows = await screen.findAllByRole("row");
  const row = rows.find((r) => r.querySelector("td span")?.textContent === name);
  if (!row) throw new Error(`No row for ${name}`);
  return row;
}

async function openMenu(name: string) {
  (await screen.findByRole("button", { name })).focus();
  await userEvent.keyboard("{Enter}");
  return screen.findByRole("menu");
}

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

describe("the Memory page", () => {
  const renderPage = () => {
    const Page = Route.options.component;
    return renderWithClient(Page ? <Page /> : null);
  };

  it("lists each type with its kind, settings, write level and agents, and marks the default", async () => {
    serve({ types: [notes, reviewed, unserved] });
    renderPage();
    const row = await rowOf("Reviewed notes");
    expect(within(row).getByText("Notes kind · up to 50 per agent, 4,000 characters recalled")).toBeVisible();
    expect(within(row).getByText("Search only")).toBeVisible();
    expect(within(row).getByText("1 agent")).toBeVisible();
    expect(within(row).queryByText("Default")).toBeNull();

    const defaultRow = await rowOf("Notes");
    expect(within(defaultRow).getByText("Default")).toBeVisible();
    expect(within(defaultRow).getByText("Allow")).toBeVisible();
    expect(within(defaultRow).getByText("None")).toBeVisible();

    const unservedRow = await rowOf("Support history");
    expect(within(unservedRow).getByRole("button", { name: "Not served" })).toBeVisible();
    expect(within(unservedRow).getByText("archive")).toBeVisible();
    expect(screen.getByText(/This server serves Notes\./)).toBeVisible();
  });

  it("lets owners and admins create, edit and delete types and choose the default", async () => {
    for (const role of ["owner", "admin"] as const) {
      scope.org = as(role);
      serve();
      renderPage();
      expect(await screen.findByRole("button", { name: "Create memory type" })).toBeVisible();
      expect(await screen.findByRole("combobox", { name: "Default memory type" })).toHaveTextContent("Notes");
      const menu = await openMenu("Actions for Reviewed notes");
      expect(within(menu).getByRole("menuitem", { name: "Edit" })).toBeVisible();
      expect(within(menu).getByRole("menuitem", { name: "Make default" })).toBeVisible();
      expect(within(menu).getByRole("menuitem", { name: "Delete memory type" })).toBeVisible();
      cleanup();
    }
  });

  it("shows developers and viewers the types and the default without a way to change them", async () => {
    for (const role of ["developer", "viewer"] as const) {
      scope.org = as(role);
      serve();
      renderPage();
      expect(await screen.findByText("Reviewed notes")).toBeVisible();
      expect(screen.getByText("Owners and admins create memory types and choose the default.")).toBeVisible();
      expect(screen.queryByRole("button", { name: "Create memory type" })).toBeNull();
      expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
      expect(screen.queryByRole("combobox", { name: "Default memory type" })).toBeNull();
      cleanup();
    }
  });

  it("says plainly that agents on the default have no memory until a default is chosen", async () => {
    serve({ types: [{ ...notes, is_default: false }, reviewed], current: null });
    renderPage();
    expect(
      await screen.findByText(
        "No default memory type, so agents on the default have no memory. Choose one to give them memory.",
      ),
    ).toBeVisible();
    expect(await screen.findByRole("combobox", { name: "Default memory type" })).toHaveTextContent("No default");

    cleanup();
    scope.org = as("viewer");
    serve({ types: [{ ...notes, is_default: false }], current: null });
    renderPage();
    expect(
      await screen.findByText(
        "No default memory type, so agents on the default have no memory. An owner or admin can choose one.",
      ),
    ).toBeVisible();
    expect(screen.getByText("None chosen")).toBeVisible();
  });

  it("asks for a first type when there are none", async () => {
    serve({ types: [], current: null });
    renderPage();
    expect(await screen.findByText("No memory types yet")).toBeVisible();
    expect(
      await screen.findByText(
        "No default memory type, so agents on the default have no memory. Create a memory type, then choose it here.",
      ),
    ).toBeVisible();
    expect(screen.getAllByRole("button", { name: "Create memory type" })).toHaveLength(2);
  });

  it("warns when the default's kind isn't served here", async () => {
    const offKind = { ...unserved, is_default: true };
    serve({ types: [offKind], current: offKind });
    renderPage();
    expect(
      await screen.findByText(/This server doesn't serve archive, the kind of Support history/, {
        selector: "span",
      }),
    ).toBeVisible();
  });

  it("chooses the default, or none", async () => {
    serve();
    api.memoryTypes.setDefault.mockResolvedValueOnce(defaultIs(reviewed)).mockResolvedValueOnce(defaultIs(null));
    renderPage();
    await userEvent.click(await screen.findByRole("combobox", { name: "Default memory type" }));
    await userEvent.click(await screen.findByRole("option", { name: /Reviewed notes/ }));
    await waitFor(() =>
      expect(api.memoryTypes.setDefault).toHaveBeenCalledWith(org.id, { memory_type_id: reviewed.id }),
    );
    await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Reviewed notes is the default memory type"));

    await userEvent.click(screen.getByRole("combobox", { name: "Default memory type" }));
    await userEvent.click(await screen.findByRole("option", { name: "No default" }));
    await waitFor(() => expect(api.memoryTypes.setDefault).toHaveBeenLastCalledWith(org.id, { memory_type_id: null }));
    await waitFor(() =>
      expect(toast.success).toHaveBeenLastCalledWith("No default memory type: agents on the default have no memory"),
    );
  });

  it("explains a refused change of the default", async () => {
    serve();
    api.memoryTypes.setDefault.mockRejectedValue(
      new ZupflowApiError(403, { code: "forbidden", message: "Only owners and admins can do this.", fields: null }),
    );
    renderPage();
    await userEvent.click(await screen.findByRole("combobox", { name: "Default memory type" }));
    await userEvent.click(await screen.findByRole("option", { name: "No default" }));
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith("Couldn't change the default memory type", {
        description: "Only owners and admins change memory types. Ask one of them.",
      }),
    );
  });

  describe("creating a type", () => {
    async function startCreate() {
      const [create] = await screen.findAllByRole("button", { name: "Create memory type" });
      if (!create) throw new Error("No way to create a memory type");
      await userEvent.click(create);
      const dialog = await screen.findByRole("dialog", { name: "Create memory type" });
      await userEvent.type(within(dialog).getByLabelText("Name"), "Team notes");
      return dialog;
    }

    it("sends the kind's settings, checked against its bounds, and the write level", async () => {
      serve();
      const created = memoryType("Team notes", { settings: { max_notes: 50, recall_characters: 8000 } });
      api.memoryTypes.create.mockResolvedValue(created);
      renderPage();
      const dialog = await startCreate();
      expect(within(dialog).getByRole("radio", { name: /^Notes/ })).toBeChecked();
      // Each setting named and explained by the kind's schema.
      const recalled = within(dialog).getByRole("spinbutton", { name: "Recall length" });
      expect(recalled).toHaveValue(8000);
      expect(recalled).toHaveAccessibleDescription(
        "How many characters of notes a run starts with. Use a whole number from 0 to 32,000. Default: 8,000.",
      );
      const kept = within(dialog).getByRole("spinbutton", { name: "Note limit" });
      expect(kept).toHaveValue(200);
      expect(kept).toHaveAccessibleDescription(
        "The most notes one agent keeps in one workspace. Use a whole number from 1 to 1,000. Default: 200.",
      );

      await userEvent.clear(kept);
      await userEvent.type(kept, "0");
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      expect(await within(dialog).findByText("Use a whole number from 1 to 1,000.")).toBeVisible();
      expect(api.memoryTypes.create).not.toHaveBeenCalled();

      await userEvent.clear(kept);
      await userEvent.type(kept, "50");
      await userEvent.click(within(dialog).getByRole("radio", { name: /^Require approval/ }));
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      await waitFor(() =>
        expect(api.memoryTypes.create).toHaveBeenCalledWith(org.id, {
          name: "Team notes",
          kind: MEMORY_KIND_NOTES,
          settings: { recall_characters: 8000, max_notes: 50 },
          writes: "approve",
        }),
      );
      await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Created Team notes"));
      // The organization has a default: making the new type the default is the panel's job.
      expect(api.memoryTypes.setDefault).not.toHaveBeenCalled();
    });

    it("builds another kind's fields from its schema, and leaves empty optional settings to the kind", async () => {
      serve();
      const archiveKind: MemoryKind = {
        kind: "archive",
        name: "Archive",
        description: "Older conversations, searched when asked.",
        settings_schema: {
          type: "object",
          required: ["language"],
          properties: {
            language: { type: "string", enum: ["english", "german"] },
            dimensions: { type: "integer", enum: [256, 768], default: 768 },
            filters: { type: "object", description: "Which conversations are kept." },
          },
        },
        manage: { list: true, add: false, change: false, forget: true },
      };
      api.memoryKinds.list.mockResolvedValue([notesKind, archiveKind]);
      api.memoryTypes.create.mockResolvedValue(
        memoryType("Team notes", { kind: "archive", settings: { language: "german", dimensions: 768 } }),
      );
      renderPage();
      const dialog = await startCreate();
      await userEvent.click(within(dialog).getByRole("radio", { name: /^Archive/ }));
      const language = within(dialog).getByRole("combobox", { name: "Language" });
      expect(within(dialog).getByRole("combobox", { name: "Dimensions" })).toHaveTextContent("768");
      expect(within(dialog).getByLabelText("Filters")).toHaveValue("");
      expect(
        within(dialog).getByText("Which conversations are kept. Leave it empty for the kind's default."),
      ).toBeVisible();

      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      expect(await within(dialog).findByText("Choose one of the options.")).toBeVisible();
      expect(api.memoryTypes.create).not.toHaveBeenCalled();

      language.focus();
      await userEvent.keyboard("{Enter}");
      await userEvent.click(await screen.findByRole("option", { name: "german" }));
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      await waitFor(() =>
        expect(api.memoryTypes.create).toHaveBeenCalledWith(org.id, {
          name: "Team notes",
          kind: "archive",
          settings: { language: "german", dimensions: 768 },
          writes: "allow",
        }),
      );
    });

    it("offers to make the first type the default while there's none", async () => {
      serve({ types: [], current: null });
      const created = memoryType("Team notes");
      api.memoryTypes.create.mockResolvedValue(created);
      api.memoryTypes.setDefault.mockResolvedValue(defaultIs(created));
      renderPage();
      const dialog = await startCreate();
      expect(within(dialog).getByRole("checkbox", { name: "Make it the default" })).toBeChecked();
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      await waitFor(() =>
        expect(api.memoryTypes.setDefault).toHaveBeenCalledWith(org.id, { memory_type_id: created.id }),
      );
      await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Created Team notes and made it the default"));
    });

    it("puts the server's refusals on their fields", async () => {
      serve();
      api.memoryTypes.create
        .mockRejectedValueOnce(
          new ZupflowApiError(409, {
            code: "memory_type_name_taken",
            message: "Another memory type is named Team notes.",
            fields: { name: "Already taken." },
          }),
        )
        .mockRejectedValueOnce(
          new ZupflowApiError(422, {
            code: "invalid_memory_settings",
            message: "The kind refused some settings.",
            fields: { "settings.max_notes": "This server keeps 100 notes at most." },
          }),
        );
      renderPage();
      const dialog = await startCreate();
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      expect(await within(dialog).findByText("Another memory type has this name. Choose another.")).toBeVisible();
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      expect(await within(dialog).findByText("This server keeps 100 notes at most.")).toBeVisible();
      expect(within(dialog).queryByRole("alert")).toBeNull();
    });

    it("says how to fix a refusal no field shows", async () => {
      serve();
      api.memoryTypes.create.mockRejectedValue(
        new ZupflowApiError(422, {
          code: "unknown_memory_kind",
          message: "This server doesn't serve that kind.",
          fields: null,
        }),
      );
      renderPage();
      const dialog = await startCreate();
      await userEvent.click(within(dialog).getByRole("button", { name: "Create memory type" }));
      expect(await within(dialog).findByRole("alert")).toHaveTextContent(
        "This server no longer serves that kind. Reload the page and choose a kind it serves.",
      );
    });
  });

  it("edits a type's name, settings and write level", async () => {
    serve();
    api.memoryTypes.update.mockResolvedValue({ ...reviewed, name: "Reviewed account notes" });
    renderPage();
    const menu = await openMenu("Actions for Reviewed notes");
    await userEvent.click(within(menu).getByRole("menuitem", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit Reviewed notes" });
    expect(within(dialog).queryByRole("radio", { name: /^Notes/ })).toBeNull();
    expect(within(dialog).getByLabelText("Note limit")).toHaveValue(50);
    const name = within(dialog).getByLabelText("Name");
    await userEvent.clear(name);
    await userEvent.type(name, "Reviewed account notes");
    await userEvent.click(within(dialog).getByRole("radio", { name: /^Allow/ }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.memoryTypes.update).toHaveBeenCalledWith(org.id, reviewed.id, {
        name: "Reviewed account notes",
        writes: "allow",
        settings: { recall_characters: 4000, max_notes: 50 },
      }),
    );
  });

  it("keeps a type whose kind isn't served as it is, sending no settings", async () => {
    serve({ types: [notes, unserved] });
    api.memoryTypes.update.mockResolvedValue(unserved);
    renderPage();
    const menu = await openMenu("Actions for Support history");
    await userEvent.click(within(menu).getByRole("menuitem", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit Support history" });
    expect(within(dialog).getByText(/This server doesn't serve archive, so it can't check settings/)).toBeVisible();
    await userEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.memoryTypes.update).toHaveBeenCalledWith(org.id, unserved.id, {
        name: "Support history",
        writes: "allow",
      }),
    );
  });

  it("says what holds a type the server won't delete, and keeps the dialog open", async () => {
    serve();
    api.memoryTypes.delete.mockRejectedValue(
      new ZupflowApiError(409, {
        code: "memory_type_in_use",
        message: "Can't delete Notes: it's the default memory type, and 2 agents use it.",
        fields: {
          default_memory_type: "Notes",
          agents: "Publishing assistant (Acme), Finance reporter (Acme)",
        },
      }),
    );
    renderPage();
    const menu = await openMenu("Actions for Notes");
    await userEvent.click(within(menu).getByRole("menuitem", { name: "Delete memory type" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete Notes?" });
    const confirm = within(dialog).getByRole("button", { name: "Delete memory type" });
    expect(confirm).toBeDisabled();
    await userEvent.type(within(dialog).getByLabelText("Type Notes to confirm"), "Notes");
    await userEvent.click(confirm);
    const alert = await within(dialog).findByRole("alert");
    expect(alert).toHaveTextContent("Notes is in use, so it wasn't deleted.");
    expect(alert).toHaveTextContent(
      "It's the organization's default memory type. Choose another default, or none, first.",
    );
    expect(alert).toHaveTextContent(
      "Agents use it: Publishing assistant (Acme), Finance reporter (Acme). In each agent's Configuration tab, choose another memory first.",
    );
    expect(screen.getByRole("alertdialog", { name: "Delete Notes?" })).toBeVisible();
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("deletes a type nothing holds", async () => {
    serve({ types: [notes, unserved] });
    api.memoryTypes.delete.mockResolvedValue(undefined);
    renderPage();
    const menu = await openMenu("Actions for Support history");
    await userEvent.click(within(menu).getByRole("menuitem", { name: "Delete memory type" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete Support history?" });
    await userEvent.type(within(dialog).getByLabelText("Type Support history to confirm"), "Support history");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete memory type" }));
    await waitFor(() => expect(api.memoryTypes.delete).toHaveBeenCalledWith(org.id, unserved.id));
    await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Deleted Support history"));
  });

  it("asks for the name again for each delete, even of a type made again with the same name", async () => {
    let listed = [notes, unserved];
    serve({ types: listed });
    api.memoryTypes.list.mockImplementation(async () => page(listed));
    api.memoryTypes.delete.mockImplementation(async () => {
      // Someone makes it again meanwhile: a new type, of the same name, of a kind this server serves.
      listed = [notes, memoryType("Support history")];
    });
    renderPage();
    const first = await openMenu("Actions for Support history");
    await userEvent.click(within(first).getByRole("menuitem", { name: "Delete memory type" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete Support history?" });
    await userEvent.type(within(dialog).getByLabelText("Type Support history to confirm"), "Support history");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete memory type" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(async () =>
      expect(within(await rowOf("Support history")).queryByRole("button", { name: "Not served" })).toBeNull(),
    );

    const again = await openMenu("Actions for Support history");
    await userEvent.click(within(again).getByRole("menuitem", { name: "Delete memory type" }));
    const second = await screen.findByRole("alertdialog", { name: "Delete Support history?" });
    expect(within(second).getByLabelText("Type Support history to confirm")).toHaveValue("");
    expect(within(second).getByRole("button", { name: "Delete memory type" })).toBeDisabled();
  });

  it("reads the types again when a refusal names what holds one", async () => {
    let listed = [notes, reviewed];
    serve({ types: listed });
    api.memoryTypes.list.mockImplementation(async () => page(listed));
    api.memoryTypes.delete.mockImplementation(async () => {
      // Another agent chose it meanwhile, which is why it's refused.
      listed = [notes, { ...reviewed, agent_count: 2 }];
      throw new ZupflowApiError(409, {
        code: "memory_type_in_use",
        message: "Can't delete Reviewed notes: 2 agents use it.",
        fields: { agents: "Publishing assistant (Acme), Product scribe (Acme)" },
      });
    });
    renderPage();
    expect(within(await rowOf("Reviewed notes")).getByText("1 agent")).toBeVisible();
    const menu = await openMenu("Actions for Reviewed notes");
    await userEvent.click(within(menu).getByRole("menuitem", { name: "Delete memory type" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete Reviewed notes?" });
    await userEvent.type(within(dialog).getByLabelText("Type Reviewed notes to confirm"), "Reviewed notes");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete memory type" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "Agents use it: Publishing assistant (Acme), Product scribe (Acme).",
    );
    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(async () => expect(within(await rowOf("Reviewed notes")).getByText("2 agents")).toBeVisible());
  });
});

describe("the agent form's memory", () => {
  function Fields({
    start,
    environment = "sandbox",
    readOnly,
  }: {
    start?: Agent;
    environment?: Workspace["environment"];
    readOnly?: boolean;
  }) {
    const form = useAgentForm(start);
    return (
      <Form {...form}>
        <form>
          <AgentFields form={form} environment={environment} readOnly={readOnly} />
        </form>
        <output data-testid="input">{JSON.stringify(agentInput(form.watch()))}</output>
      </Form>
    );
  }
  const sent = () => JSON.parse(screen.getByTestId("input").textContent ?? "{}");

  it("starts on the organization's default, naming its type", async () => {
    serve();
    renderWithClient(<Fields />);
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Memory" })).toHaveTextContent("(Notes)"));
    expect(screen.getByRole("combobox", { name: "Memory" })).toHaveTextContent("Organization default");
    expect(sent()).toMatchObject({ memory: "default", memory_type_id: null });
    expect(screen.queryByRole("link", { name: "Open memory" })).toBeNull();
  });

  it("says when the organization has no default, and who can choose one", async () => {
    serve({ current: null });
    renderWithClient(<Fields />);
    await waitFor(() =>
      expect(screen.getByRole("combobox", { name: "Memory" })).toHaveTextContent("Organization default (none chosen)"),
    );
    expect(
      await screen.findByText(
        "Fernhill has no default memory type, so this agent runs without memory. Choose a default on the Memory page, or a memory type for this agent.",
      ),
    ).toBeVisible();
    expect(screen.getByRole("link", { name: "Open memory" })).toHaveAttribute("href", "/fernhill/memory");

    cleanup();
    scope.org = as("developer");
    serve({ current: null });
    renderWithClient(<Fields />);
    expect(
      await screen.findByText(
        "Fernhill has no default memory type, so this agent runs without memory. Ask an owner or admin to choose a default, or choose a memory type for this agent.",
      ),
    ).toBeVisible();
  });

  it("chooses one of the organization's types, or off", async () => {
    serve({ types: [notes, reviewed, unserved] });
    renderWithClient(<Fields />);
    const picker = await screen.findByRole("combobox", { name: "Memory" });
    await userEvent.click(picker);
    expect(await screen.findByRole("option", { name: /Organization default/ })).toBeVisible();
    await userEvent.click(await screen.findByRole("option", { name: /Reviewed notes/ }));
    expect(sent()).toMatchObject({ memory: "type", memory_type_id: reviewed.id });
    expect(picker).toHaveTextContent("Reviewed notes");

    await userEvent.click(picker);
    await userEvent.click(await screen.findByRole("option", { name: /Support history/ }));
    expect(await screen.findByText(/This server doesn't serve archive, the kind of Support history/)).toBeVisible();

    await userEvent.click(picker);
    await userEvent.click(await screen.findByRole("option", { name: "Off" }));
    expect(sent()).toMatchObject({ memory: "off", memory_type_id: null });
    expect(picker).toHaveTextContent("Off");
    expect(screen.queryByText(/This server doesn't serve/)).toBeNull();
  });

  it("starts from the agent's own choice", async () => {
    serve();
    renderWithClient(<Fields start={{ ...agent, memory: "type", memory_type_id: reviewed.id }} />);
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Memory" })).toHaveTextContent("Reviewed notes"));
    expect(sent()).toMatchObject({ memory: "type", memory_type_id: reviewed.id });
  });

  it("finds a choice by the start of its words: Off for off, never the organization default", async () => {
    serve();
    renderWithClient(<Fields />);
    await userEvent.click(await screen.findByRole("combobox", { name: "Memory" }));
    await userEvent.type(await screen.findByPlaceholderText("Search memory types"), "off");
    expect(await screen.findByRole("option", { name: "Off" })).toHaveAttribute("aria-selected", "true");
    expect(screen.queryByRole("option", { name: /Organization default/ })).toBeNull();
    await userEvent.keyboard("{Enter}");
    expect(sent()).toMatchObject({ memory: "off", memory_type_id: null });

    await userEvent.click(screen.getByRole("combobox", { name: "Memory" }));
    await userEvent.type(await screen.findByPlaceholderText("Search memory types"), "org def");
    expect(await screen.findByRole("option", { name: /Organization default/ })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.queryByRole("option", { name: "Off" })).toBeNull();
  });

  it("shows viewers the choice without a way to change it, and who can", async () => {
    scope.org = as("viewer");
    serve({ current: null });
    renderWithClient(<Fields start={agent} readOnly />);
    const picker = await screen.findByRole("combobox", { name: "Memory" });
    await waitFor(() => expect(picker).toHaveTextContent("Organization default (none chosen)"));
    expect(picker).toBeDisabled();
    expect(
      await screen.findByText(
        "Fernhill has no default memory type, so this agent runs without memory. Ask an owner or admin to choose a default, or a developer to choose a memory type for this agent.",
      ),
    ).toBeVisible();
  });

  it("chooses an agent's memory in Production as in Sandbox", async () => {
    scope.org = as("developer");
    serve();
    renderWithClient(<Fields start={{ ...agent, workspace_id: production.id }} environment="production" />);
    const picker = await screen.findByRole("combobox", { name: "Memory" });
    expect(picker).toBeEnabled();
    await userEvent.click(picker);
    await userEvent.click(await screen.findByRole("option", { name: /Reviewed notes/ }));
    expect(sent()).toMatchObject({ memory: "type", memory_type_id: reviewed.id });
  });
});

describe("an agent's memories", () => {
  const renderMemories = ({
    workspace = sandbox,
    start = agent,
    live = false,
  }: {
    workspace?: Workspace;
    start?: Agent;
    live?: boolean;
  } = {}) => renderWithClient(<AgentMemories org={scope.org} workspace={workspace} agent={start} />, { live });

  it("lists them as the server orders them, with who wrote each and its marks", async () => {
    serve();
    renderMemories();
    const rows = await screen.findAllByRole("listitem");
    expect(rows.map((row) => row.querySelector("p")?.textContent)).toEqual([
      britishSpelling.text,
      shortPosts.text,
      drafts.text,
    ]);
    expect(api.memories.list).toHaveBeenCalledWith(
      sandbox.id,
      agent.id,
      expect.objectContaining({ memory_type: notes.id, limit: 25 }),
    );
    expect(screen.getByText("Publishing assistant remembers with", { exact: false })).toBeVisible();
    expect(screen.getByText("3 of 200")).toBeVisible();

    const [run, apiKey, person] = rows as [HTMLElement, HTMLElement, HTMLElement];
    expect(within(run).getByText("k7m2qd")).toBeVisible();
    expect(within(run).getByText(/^Changed/)).toBeVisible();
    expect(within(run).getByRole("link", { name: shortId(runId) })).toHaveAttribute(
      "href",
      `/fernhill/w/${sandbox.id}/runs/${runId}`,
    );
    expect(within(run).getByRole("button", { name: "After app results" })).toBeVisible();
    expect(within(run).getByRole("button", { name: "Direct conversation" })).toBeVisible();
    expect(within(apiKey).getByText(/^Saved/)).toBeVisible();
    expect(await within(apiKey).findByText("Queue worker")).toBeVisible();
    expect(within(apiKey).queryByRole("button", { name: "After app results" })).toBeNull();
    expect(within(apiKey).queryByRole("button", { name: "Direct conversation" })).toBeNull();
    expect(await within(person).findByText("Jordan Lee")).toBeVisible();
  });

  it("searches them on the server", async () => {
    serve();
    renderMemories();
    await screen.findByText(drafts.text);
    api.memories.list.mockResolvedValue(page([]));
    await userEvent.type(screen.getByPlaceholderText("Search memories"), "brand voice");
    await waitFor(() =>
      expect(api.memories.list).toHaveBeenCalledWith(
        sandbox.id,
        agent.id,
        expect.objectContaining({ q: "brand voice" }),
      ),
    );
    expect(await screen.findByText("No memory holds every word of “brand voice”.")).toBeVisible();
  });

  it("pages older ones", async () => {
    serve();
    const older = memory("old2ab", "Older note about the spring newsletter.", { type: "member", user_id: jordan.id });
    api.memories.list.mockImplementation(async (_ws, _agent, query: { cursor?: string }) =>
      query.cursor === "page-2" ? page([older], null, 4) : page([britishSpelling, shortPosts, drafts], "page-2", 4),
    );
    renderMemories();
    await userEvent.click(await screen.findByRole("button", { name: "Load older memories" }));
    expect(await screen.findByText(older.text)).toBeVisible();
    expect(api.memories.list).toHaveBeenLastCalledWith(
      sandbox.id,
      agent.id,
      expect.objectContaining({ cursor: "page-2" }),
    );
    expect(screen.queryByRole("button", { name: "Load older memories" })).toBeNull();
  });

  describe("for developers and up in Sandbox", () => {
    beforeEach(() => {
      scope.org = as("developer");
    });

    it("adds a memory", async () => {
      serve();
      api.memories.create.mockResolvedValue(memory("new2ab", "Prefers headlines in sentence case.", fromRun));
      renderMemories();
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      await userEvent.type(within(dialog).getByLabelText("Memory"), "Prefers headlines in sentence case.");
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      await waitFor(() =>
        expect(api.memories.create).toHaveBeenCalledWith(sandbox.id, agent.id, {
          text: "Prefers headlines in sentence case.",
          memory_type_id: notes.id,
        }),
      );
      await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Saved memory new2ab"));
    });

    it("changes a memory", async () => {
      serve();
      api.memories.update.mockResolvedValue({ ...drafts, text: "Send drafts as a link." });
      renderMemories();
      const menu = await openMenu("Actions for memory draft3");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Change" }));
      const dialog = await screen.findByRole("dialog", { name: "Change memory" });
      const field = within(dialog).getByLabelText("Memory");
      expect(field).toHaveValue(drafts.text);
      await userEvent.clear(field);
      await userEvent.type(field, "Send drafts as a link.");
      await userEvent.click(within(dialog).getByRole("button", { name: "Save memory" }));
      await waitFor(() =>
        expect(api.memories.update).toHaveBeenCalledWith(sandbox.id, agent.id, drafts.id, {
          text: "Send drafts as a link.",
        }),
      );
    });

    it("forgets a memory", async () => {
      serve();
      api.memories.delete.mockResolvedValue(undefined);
      renderMemories();
      const menu = await openMenu("Actions for memory tone2a");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget" }));
      const dialog = await screen.findByRole("alertdialog", { name: "Forget this memory?" });
      expect(within(dialog).getByText(shortPosts.text)).toBeVisible();
      await userEvent.click(within(dialog).getByRole("button", { name: "Forget memory" }));
      await waitFor(() => expect(api.memories.delete).toHaveBeenCalledWith(sandbox.id, agent.id, shortPosts.id));
      await waitFor(() => expect(toast.success).toHaveBeenCalledWith("Forgot memory tone2a"));
    });

    it("forgets them all once the agent's name is typed", async () => {
      serve();
      api.memories.deleteAll.mockResolvedValue(undefined);
      renderMemories();
      const menu = await openMenu("More memory actions");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
      const dialog = await screen.findByRole("alertdialog", {
        name: "Forget everything Publishing assistant remembers here?",
      });
      const confirm = within(dialog).getByRole("button", { name: "Forget all memories" });
      expect(confirm).toBeDisabled();
      await userEvent.type(within(dialog).getByLabelText("Type Publishing assistant to confirm"), agent.name);
      await userEvent.click(confirm);
      await waitFor(() => expect(api.memories.deleteAll).toHaveBeenCalledWith(sandbox.id, agent.id));
      await waitFor(() =>
        expect(toast.success).toHaveBeenCalledWith("Forgot everything Publishing assistant kept in Acme"),
      );
    });

    it("checks a note's length before sending it, and shows the kind's refusal on the text", async () => {
      serve();
      api.memories.create.mockRejectedValue(
        new ZupflowApiError(422, {
          code: "invalid_memory_text",
          message: "The memory's text isn't one the kind keeps.",
          fields: { text: "Write a note of 1 to 500 characters on one line." },
        }),
      );
      renderMemories();
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      const field = within(dialog).getByLabelText("Memory");
      await userEvent.click(field);
      await userEvent.paste("x".repeat(501));
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      expect(within(dialog).getByText("Use 500 characters or fewer. It has 501.")).toBeVisible();
      expect(api.memories.create).not.toHaveBeenCalled();

      await userEvent.clear(field);
      await userEvent.type(field, "A note the kind refuses.");
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      expect(await within(dialog).findByText("Write a note of 1 to 500 characters on one line.")).toBeVisible();
      expect(within(dialog).queryByRole("alert")).toBeNull();
    });

    it("explains memory_off, memory_full and the kind's refusals", async () => {
      const refusals: [ZupflowApiError, string][] = [
        [
          new ZupflowApiError(422, { code: "memory_off", message: "This agent has no memory.", fields: null }),
          "Publishing assistant uses the organization's default memory type, and none is chosen. Ask an owner or admin to choose one, or choose a memory type for Publishing assistant in its Configuration tab.",
        ],
        [
          new ZupflowApiError(422, {
            code: "memory_full",
            message: "This agent keeps 200 memories at most.",
            fields: null,
          }),
          "Publishing assistant keeps as many memories as Notes allows. Forget one, or change one instead of adding another.",
        ],
        [
          new ZupflowApiError(422, {
            code: "memory_kind_not_served",
            message: "This server doesn't serve notes.",
            fields: null,
          }),
          "This server doesn't serve notes, the kind of Notes, so its memories can't be saved or changed here. Ask whoever runs this server to serve it, or choose another memory type for Publishing assistant.",
        ],
        [
          new ZupflowApiError(422, {
            code: "memory_kind_unsupported",
            message: "The notes kind doesn't take memories from people.",
            fields: null,
          }),
          "The kind of Notes (notes) doesn't let people do that with its memories from the Console or the API.",
        ],
      ];
      for (const [error, copy] of refusals) {
        serve();
        api.memories.create.mockRejectedValue(error);
        renderMemories();
        await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
        const dialog = await screen.findByRole("dialog", { name: "Add memory" });
        await userEvent.type(within(dialog).getByLabelText("Memory"), "Prefers short sentences.");
        await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
        expect(await within(dialog).findByRole("alert")).toHaveTextContent(copy);
        cleanup();
      }
    });

    it("says when it's full, and offers no way to add", async () => {
      serve();
      api.memories.list.mockResolvedValue(page([britishSpelling, shortPosts], "page-2", 200));
      renderMemories();
      expect(await screen.findByText("200 of 200")).toBeVisible();
      expect(
        screen.getByText(
          "Publishing assistant keeps 200 memories at most in Notes, and it's full: its runs can't save more until you forget one, or change one instead.",
        ),
      ).toBeVisible();
      expect(screen.getByRole("button", { name: "Add memory" })).toBeDisabled();
    });
    it("adds to a type it's looking in, naming that type", async () => {
      serve();
      api.memories.create.mockResolvedValue(
        memory(
          "rev1ab",
          "Quotes need a source.",
          { type: "member", user_id: jordan.id },
          { memory_type_id: reviewed.id },
        ),
      );
      renderMemories();
      await userEvent.click(await screen.findByRole("combobox", { name: "Memory type shown" }));
      await userEvent.click(await screen.findByRole("option", { name: /Reviewed notes/ }));
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      expect(
        within(dialog).getByText(
          "For Publishing assistant in Acme, in Reviewed notes, which it doesn't use now: its runs recall this only once it does.",
        ),
      ).toBeVisible();
      await userEvent.type(within(dialog).getByLabelText("Memory"), "Quotes need a source.");
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      await waitFor(() =>
        expect(api.memories.create).toHaveBeenCalledWith(sandbox.id, agent.id, {
          text: "Quotes need a source.",
          memory_type_id: reviewed.id,
        }),
      );
    });

    it("reads the list again after a refused save", async () => {
      serve();
      let held = page([britishSpelling, shortPosts, drafts]);
      api.memories.list.mockImplementation(async () => held);
      api.memories.create.mockImplementation(async () => {
        // Its runs filled it meanwhile, which is why this one is refused.
        held = page([britishSpelling, shortPosts], "page-2", 200);
        throw new ZupflowApiError(422, { code: "memory_full", message: "This agent keeps 200 at most.", fields: null });
      });
      renderMemories({ live: true });
      expect(await screen.findByText("3 of 200")).toBeVisible();
      await userEvent.click(screen.getByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      await userEvent.type(within(dialog).getByLabelText("Memory"), "Prefers short sentences.");
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      expect(await within(dialog).findByRole("alert")).toHaveTextContent(
        "Publishing assistant keeps as many memories as Notes allows.",
      );
      await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
      expect(await screen.findByText("200 of 200")).toBeVisible();
      expect(screen.getByRole("button", { name: "Add memory" })).toBeDisabled();
    });

    it("saves once when the answer is lost on the way: the SDK's retry with the same key gets the memory", async () => {
      serve();
      let held = page([britishSpelling, shortPosts]);
      api.memories.list.mockImplementation(async () => held);
      const saved = memory("week5d", "Send the weekly digest on Fridays.", { type: "member", user_id: jordan.id });
      // The SDK as the Console uses it, over a network that loses the first answer.
      const keys: (string | null)[] = [];
      const network = vi.fn(async (_url: RequestInfo | URL, init?: RequestInit) => {
        keys.push(new Headers(init?.headers).get(IDEMPOTENCY_HEADER));
        if (keys.length === 1) {
          // The server saved it, and its answer never arrived.
          held = page([saved, britishSpelling, shortPosts]);
          throw new TypeError("Failed to fetch");
        }
        // The retry: the memory that key saved, as it is now.
        return new Response(JSON.stringify({ data: saved, meta: null, error: null }), {
          status: 201,
          headers: { "Content-Type": "application/json" },
        });
      });
      const sdk = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => null,
          fetch: network as unknown as typeof fetch,
        }),
      );
      api.memories.create.mockImplementation((...args: Parameters<typeof sdk.memories.create>) =>
        sdk.memories.create(...args),
      );
      renderMemories({ live: true });
      expect(await screen.findByText("2 of 200")).toBeVisible();
      await userEvent.click(screen.getByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      await userEvent.type(within(dialog).getByLabelText("Memory"), saved.text);
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      expect(keys).toHaveLength(2);
      expect(keys[0]).toBeTruthy();
      expect(keys[1]).toBe(keys[0]);
      expect(toast.success).toHaveBeenCalledWith("Saved memory week5d");
      expect(toast.error).not.toHaveBeenCalled();
      expect(await screen.findByText("3 of 200")).toBeVisible();
      expect(screen.getAllByText(saved.text)).toHaveLength(1);
      expect(screen.queryByRole("alert")).toBeNull();
    });

    it("asks for the agent's name again each time everything is forgotten", async () => {
      serve();
      api.memories.deleteAll.mockResolvedValue(undefined);
      renderMemories();
      for (const time of [1, 2]) {
        const menu = await openMenu("More memory actions");
        await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
        const dialog = await screen.findByRole("alertdialog", {
          name: "Forget everything Publishing assistant remembers here?",
        });
        const field = within(dialog).getByLabelText("Type Publishing assistant to confirm");
        const confirm = within(dialog).getByRole("button", { name: "Forget all memories" });
        expect(field).toHaveValue("");
        expect(confirm).toBeDisabled();
        await userEvent.type(field, agent.name);
        await userEvent.click(confirm);
        await waitFor(() => expect(api.memories.deleteAll).toHaveBeenCalledTimes(time));
        await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
      }
    });

    it("says what forgetting everything forgets, and that memory stays on only when it's on", async () => {
      serve();
      renderMemories();
      let menu = await openMenu("More memory actions");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
      let dialog = await screen.findByRole("alertdialog");
      expect(dialog).toHaveTextContent(
        "Every memory Publishing assistant keeps in Acme is forgotten, in each memory type of a kind this server serves, and webhooks get memory.forgotten for each. Its memory stays on, so its runs start remembering again. This can't be undone.",
      );
      cleanup();

      serve();
      renderMemories({ start: { ...agent, memory: "off" } });
      menu = await openMenu("More memory actions");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
      dialog = await screen.findByRole("alertdialog");
      expect(dialog).toHaveTextContent("for each. This can't be undone.");
      expect(dialog).not.toHaveTextContent("stays on");
    });

    it("explains a refused change or forget: one viewers can't make, and a memory already gone", async () => {
      const forbidden = new ZupflowApiError(403, {
        code: "forbidden",
        message: "Viewers can't change memories.",
        fields: null,
      });
      const gone = new ZupflowApiError(404, { code: "not_found", message: "Memory was not found.", fields: null });
      serve();
      api.memories.update.mockRejectedValueOnce(forbidden).mockRejectedValueOnce(gone);
      api.memories.delete.mockRejectedValueOnce(forbidden).mockRejectedValueOnce(gone);
      renderMemories();

      await userEvent.click(
        within(await openMenu("Actions for memory draft3")).getByRole("menuitem", { name: "Change" }),
      );
      const change = await screen.findByRole("dialog", { name: "Change memory" });
      await userEvent.type(within(change).getByLabelText("Memory"), " Always.");
      await userEvent.click(within(change).getByRole("button", { name: "Save memory" }));
      expect(await within(change).findByRole("alert")).toHaveTextContent(
        "Viewers can't change memories. Ask a developer, an admin or an owner.",
      );
      await userEvent.click(within(change).getByRole("button", { name: "Save memory" }));
      await waitFor(() =>
        expect(within(change).getByRole("alert")).toHaveTextContent(
          "This memory was already forgotten. The list now shows what's left.",
        ),
      );
      await userEvent.click(within(change).getByRole("button", { name: "Cancel" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

      await userEvent.click(
        within(await openMenu("Actions for memory tone2a")).getByRole("menuitem", { name: "Forget" }),
      );
      const forget = await screen.findByRole("alertdialog", { name: "Forget this memory?" });
      await userEvent.click(within(forget).getByRole("button", { name: "Forget memory" }));
      expect(await within(forget).findByRole("alert")).toHaveTextContent(
        "Viewers can't change memories. Ask a developer, an admin or an owner.",
      );
      await userEvent.click(within(forget).getByRole("button", { name: "Forget memory" }));
      await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
      expect(toast.success).toHaveBeenCalledWith("Memory tone2a was already forgotten");
    });

    it("says the agent is gone when saving, or forgetting everything, finds no agent", async () => {
      const gone = new ZupflowApiError(404, { code: "not_found", message: "Agent was not found.", fields: null });
      serve();
      api.memories.create.mockRejectedValue(gone);
      api.memories.deleteAll.mockRejectedValue(gone);
      renderMemories();
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const add = await screen.findByRole("dialog", { name: "Add memory" });
      await userEvent.type(within(add).getByLabelText("Memory"), "Prefers short sentences.");
      await userEvent.click(within(add).getByRole("button", { name: "Add memory" }));
      expect(await within(add).findByRole("alert")).toHaveTextContent(
        "Publishing assistant was deleted meanwhile, with its memories, so nothing was saved.",
      );
      await userEvent.click(within(add).getByRole("button", { name: "Cancel" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

      const menu = await openMenu("More memory actions");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
      const all = await screen.findByRole("alertdialog");
      await userEvent.type(within(all).getByLabelText("Type Publishing assistant to confirm"), agent.name);
      await userEvent.click(within(all).getByRole("button", { name: "Forget all memories" }));
      expect(await within(all).findByRole("alert")).toHaveTextContent(
        "Publishing assistant was deleted meanwhile, with its memories: nothing is left to forget.",
      );
    });

    it("says forgetting everything cut off partway may have forgotten some, and forgets the rest when asked again", async () => {
      serve();
      let held = page([britishSpelling, shortPosts, drafts]);
      api.memories.list.mockImplementation(async () => held);
      api.memories.deleteAll
        .mockImplementationOnce(async () => {
          // A hundred at a time: those before the connection was cut are gone.
          held = page([britishSpelling, shortPosts]);
          throw new ZupflowNetworkError(new TypeError("Failed to fetch"));
        })
        .mockImplementationOnce(async () => {
          held = page([britishSpelling]);
          throw new ZupflowApiError(503, { code: "unavailable", message: "The server is unavailable.", fields: null });
        })
        .mockImplementationOnce(async () => {
          held = page([]);
        });
      renderMemories({ live: true });
      expect(await screen.findByText("3 of 200")).toBeVisible();
      const menu = await openMenu("More memory actions");
      await userEvent.click(within(menu).getByRole("menuitem", { name: "Forget all memories" }));
      const dialog = await screen.findByRole("alertdialog");
      await userEvent.type(within(dialog).getByLabelText("Type Publishing assistant to confirm"), agent.name);
      const confirm = within(dialog).getByRole("button", { name: "Forget all memories" });
      const partway =
        "Some of its memories may already be forgotten: the list shows what's left, and forgetting all again forgets the rest.";
      await userEvent.click(confirm);
      const alert = await within(dialog).findByRole("alert");
      expect(alert).toHaveTextContent(/^Can't reach Zupflow\./);
      expect(alert).toHaveTextContent(partway);
      // The list behind the dialog is read again: what's left.
      expect(await screen.findByText("2 of 200")).toBeInTheDocument();
      await userEvent.click(confirm);
      await waitFor(() =>
        expect(within(dialog).getByRole("alert")).toHaveTextContent(`The server is unavailable. ${partway}`),
      );
      await userEvent.click(confirm);
      await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
      expect(api.memories.deleteAll).toHaveBeenCalledTimes(3);
      expect(toast.success).toHaveBeenCalledWith("Forgot everything Publishing assistant kept in Acme");
    });

    it("counts a note as the kind keeps it: each line break a space, none collapsed", async () => {
      serve();
      renderMemories();
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      const field = within(dialog).getByLabelText("Memory");
      await userEvent.click(field);
      await userEvent.paste(`${"x".repeat(250)}${"\n".repeat(10)}${"x".repeat(245)}`);
      expect(within(dialog).getByText("505 / 500")).toBeVisible();
      await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
      expect(within(dialog).getByText("Use 500 characters or fewer. It has 505.")).toBeVisible();
      expect(api.memories.create).not.toHaveBeenCalled();

      await userEvent.clear(field);
      await userEvent.paste("Prefers\r\nshort\tsentences.​  ");
      expect(within(dialog).getByText("24 / 500")).toBeVisible();
    });

    it("saves nothing on the Enter that ends an input method's composing", async () => {
      serve();
      api.memories.create.mockResolvedValue(
        memory("ime1ab", "日本語で返信する", { type: "member", user_id: jordan.id }),
      );
      renderMemories();
      await userEvent.click(await screen.findByRole("button", { name: "Add memory" }));
      const dialog = await screen.findByRole("dialog", { name: "Add memory" });
      const field = within(dialog).getByLabelText("Memory");
      await userEvent.click(field);
      await userEvent.paste("日本語で返信する");
      fireEvent.keyDown(field, { key: "Enter", isComposing: true });
      expect(api.memories.create).not.toHaveBeenCalled();
      fireEvent.keyDown(field, { key: "Enter" });
      await waitFor(() =>
        expect(api.memories.create).toHaveBeenCalledWith(sandbox.id, agent.id, {
          text: "日本語で返信する",
          memory_type_id: notes.id,
        }),
      );
    });
  });

  it("shows them read-only in Production, for every role", async () => {
    for (const role of ["owner", "admin", "developer", "viewer"] as const) {
      scope.org = as(role);
      serve({ memories: [shortPosts] });
      renderMemories({ workspace: production, start: { ...agent, workspace_id: production.id } });
      expect(await screen.findByText(shortPosts.text)).toBeVisible();
      expect(screen.getByText(PRODUCTION_MEMORIES)).toBeVisible();
      expect(screen.queryByRole("button", { name: "Add memory" })).toBeNull();
      expect(screen.queryByRole("button", { name: "More memory actions" })).toBeNull();
      expect(screen.queryByRole("button", { name: /^Actions for memory/ })).toBeNull();
      cleanup();
    }
  });

  it("shows viewers them read-only", async () => {
    scope.org = as("viewer");
    serve();
    renderMemories();
    expect(await screen.findByText(drafts.text)).toBeVisible();
    expect(
      screen.getByText("You can view memories. Owners, admins and developers can add, change and forget them."),
    ).toBeVisible();
    expect(screen.queryByRole("button", { name: "Add memory" })).toBeNull();
    expect(screen.queryByRole("button", { name: "More memory actions" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for memory/ })).toBeNull();
  });

  it("says why an agent remembers nothing: memory off, or no default", async () => {
    serve();
    renderMemories({ start: { ...agent, memory: "off" } });
    expect(
      await screen.findByText(
        "Publishing assistant's memory is off: its runs neither recall nor save memories. Turn it on in its Configuration tab. What it kept before stays until it's forgotten.",
      ),
    ).toBeVisible();
    expect(api.memories.list).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Add memory" })).toBeNull();

    cleanup();
    serve({ current: null });
    renderMemories();
    expect(
      await screen.findByText(
        "Publishing assistant uses the organization's default memory type, and none is chosen. Choose one on the Memory page, or choose a memory type for Publishing assistant in its Configuration tab.",
      ),
    ).toBeVisible();
    expect(api.memories.list).not.toHaveBeenCalled();
  });

  it("explains a type whose kind isn't served, or doesn't let people list it", async () => {
    serve({ types: [notes, unserved] });
    renderMemories({ start: { ...agent, memory: "type", memory_type_id: unserved.id } });
    expect(
      await screen.findByText("This server doesn't serve archive, so Support history's memories can't be read here."),
    ).toBeVisible();
    expect(api.memories.list).not.toHaveBeenCalled();

    cleanup();
    serve();
    api.memories.list.mockRejectedValue(
      new ZupflowApiError(422, {
        code: "memory_kind_unsupported",
        message: "The kind doesn't list memories.",
        fields: null,
      }),
    );
    renderMemories();
    expect(
      await screen.findByText("The kind of Notes doesn't let people list its memories from the Console or the API."),
    ).toBeVisible();
  });
  it("explains each mark when it's tapped", async () => {
    serve();
    renderMemories();
    const [run] = await screen.findAllByRole("listitem");
    if (!run) throw new Error("No memories listed");
    await userEvent.click(within(run).getByRole("button", { name: "After app results" }));
    expect(await screen.findByRole("dialog", { name: "After app results" })).toHaveTextContent(
      "The run had read an app's results before it wrote this, so the text may come from that app's content. Check it before relying on it.",
    );
    await userEvent.keyboard("{Escape}");
    await userEvent.click(within(run).getByRole("button", { name: "Direct conversation" }));
    expect(await screen.findByRole("dialog", { name: "Direct conversation" })).toHaveTextContent(
      "Saved in a direct conversation: recalled and found only there.",
    );
  });

  it("says a member or an API key wrote one when their names can't be read", async () => {
    const refused = new ZupflowApiError(403, { code: "forbidden", message: "Not allowed.", fields: null });
    serve();
    api.members.list.mockRejectedValue(refused);
    api.apiKeys.list.mockRejectedValue(refused);
    renderMemories();
    const [, apiKey, person] = (await screen.findAllByRole("listitem")) as [HTMLElement, HTMLElement, HTMLElement];
    expect(await within(apiKey).findByText("an API key")).toBeVisible();
    expect(await within(person).findByText("a member")).toBeVisible();
  });

  it("follows the agent to the type it uses now, counting only that type's memories", async () => {
    scope.org = as("developer");
    const short = memoryType("Short notes", { settings: { max_notes: 2, recall_characters: 1000 } });
    const general = memory(
      "gen1ab",
      "Writes for a general audience.",
      { type: "member", user_id: jordan.id },
      {
        memory_type_id: short.id,
      },
    );
    const extra = memory("par4ab", "Prefers short paragraphs.", { type: "member", user_id: jordan.id });
    const held: Record<string, Memory[]> = {
      [notes.id]: [britishSpelling, shortPosts, drafts, extra],
      [short.id]: [general],
    };
    serve({ types: [notes, short, unserved] });
    api.memories.list.mockImplementation(async (_ws, _agent, query: { memory_type?: string }) =>
      page(held[query.memory_type ?? ""] ?? []),
    );
    api.memories.create.mockResolvedValue(
      memory("new2ab", "Prefers plain words.", { type: "member", user_id: jordan.id }, { memory_type_id: short.id }),
    );
    const onNotes: Agent = { ...agent, memory: "type", memory_type_id: notes.id };
    const view = renderMemories({ start: onNotes, live: true });
    expect(await screen.findByText("4 of 200")).toBeVisible();

    // Its Configuration tab moved it to another type.
    view.change(<AgentMemories org={scope.org} workspace={sandbox} agent={{ ...onNotes, memory_type_id: short.id }} />);
    expect(await screen.findByText(general.text)).toBeVisible();
    expect(screen.queryByText(britishSpelling.text)).toBeNull();
    expect(screen.getByText("1 of 2")).toBeVisible();
    expect(screen.queryByText(/and it's full/)).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Add memory" }));
    const dialog = await screen.findByRole("dialog", { name: "Add memory" });
    await userEvent.type(within(dialog).getByLabelText("Memory"), "Prefers plain words.");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add memory" }));
    await waitFor(() =>
      expect(api.memories.create).toHaveBeenCalledWith(sandbox.id, agent.id, {
        text: "Prefers plain words.",
        memory_type_id: short.id,
      }),
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    // Then to a type of a kind this server doesn't serve: nothing listed, nothing counted.
    view.change(
      <AgentMemories org={scope.org} workspace={sandbox} agent={{ ...onNotes, memory_type_id: unserved.id }} />,
    );
    expect(
      await screen.findByText("This server doesn't serve archive, so Support history's memories can't be read here."),
    ).toBeVisible();
    expect(screen.queryByText(general.text)).toBeNull();
    expect(screen.queryByText(/^\d+ (of \d+|memory|memories)$/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Add memory" })).toBeNull();
  });

  it("never stands one type's memories in for another's while they load, but keeps the last search's", async () => {
    const checked = memory(
      "rev1ab",
      "Quotes need a source.",
      { type: "member", user_id: jordan.id },
      {
        memory_type_id: reviewed.id,
      },
    );
    const reviewedPage = deferred<ReturnType<typeof page<Memory>>>();
    const searched = deferred<ReturnType<typeof page<Memory>>>();
    serve();
    api.memories.list.mockImplementation(async (_ws, _agent, query: { memory_type?: string; q?: string }) => {
      if (query.memory_type !== reviewed.id) return page([britishSpelling, shortPosts, drafts]);
      return query.q ? searched.promise : reviewedPage.promise;
    });
    renderMemories({ live: true });
    expect(await screen.findByText(drafts.text)).toBeVisible();

    await userEvent.click(screen.getByRole("combobox", { name: "Memory type shown" }));
    await userEvent.click(await screen.findByRole("option", { name: /Reviewed notes/ }));
    expect(await screen.findByRole("status", { name: "Loading" })).toBeVisible();
    expect(screen.queryByText(drafts.text)).toBeNull();
    expect(screen.queryByText(/ of 200$/)).toBeNull();
    reviewedPage.settle(page([checked]));
    expect(await screen.findByText(checked.text)).toBeVisible();
    expect(screen.getByText("1 of 50")).toBeVisible();

    await userEvent.type(screen.getByPlaceholderText("Search memories"), "brand");
    await waitFor(() =>
      expect(api.memories.list).toHaveBeenCalledWith(
        sandbox.id,
        agent.id,
        expect.objectContaining({ memory_type: reviewed.id, q: "brand" }),
      ),
    );
    // The last results stay while the search runs, but no count stands for them.
    expect(screen.getByText(checked.text)).toBeVisible();
    expect(screen.queryByText("1 of 50")).toBeNull();
    searched.settle(page([]));
    expect(await screen.findByText("No memory holds every word of “brand”.")).toBeVisible();
    expect(screen.getByText("0 found")).toBeVisible();
  });

  it("says where a search-only type's memories come from, for who's looking and where", async () => {
    const searchOnly = { ...notes, writes: "deny" as const };
    const cases: [Organization, Workspace, string][] = [
      [as("developer"), sandbox, "Its runs only search their memories. Add them here, or through the API."],
      [
        as("viewer"),
        sandbox,
        "Its runs only search their memories, which owners, admins and developers add here or through the API.",
      ],
      [as("owner"), production, "Its runs only search their memories, which are added through the API."],
    ];
    for (const [who, workspace, copy] of cases) {
      scope.org = who;
      serve({ types: [searchOnly, reviewed], current: searchOnly, memories: [] });
      renderMemories({ workspace, start: { ...agent, workspace_id: workspace.id } });
      expect(await screen.findByText(copy)).toBeVisible();
      cleanup();
    }
  });

  it("tells viewers who can give an agent memory", async () => {
    scope.org = as("viewer");
    serve();
    renderMemories({ start: { ...agent, memory: "off" } });
    expect(
      await screen.findByText(
        "Publishing assistant's memory is off: its runs neither recall nor save memories. An owner, an admin or a developer can turn it on in its Configuration tab. What it kept before stays until it's forgotten.",
      ),
    ).toBeVisible();

    cleanup();
    serve({ current: null });
    renderMemories();
    expect(
      await screen.findByText(
        "Publishing assistant uses the organization's default memory type, and none is chosen. Ask an owner or admin to choose one, or a developer to choose a memory type for Publishing assistant.",
      ),
    ).toBeVisible();
  });
});
