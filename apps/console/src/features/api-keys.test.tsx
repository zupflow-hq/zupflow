import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ApiKey, CreatedApiKey, Environment, Organization } from "@zupflow/sdk";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useCreateApiKey } from "@/features/queries";
import { qk } from "@/lib/query-keys";
import { newId } from "@/mocks/ids";
import { Route } from "@/routes/_authed/$org/api-keys";

const { api, scope } = vi.hoisted(() => ({
  api: { apiKeys: { list: vi.fn(), create: vi.fn(), revoke: vi.fn() } },
  scope: {} as { org: Organization; environment: Environment },
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));
vi.mock("@/features/context", async (original) => ({
  ...(await original<typeof import("@/features/context")>()),
  useOrg: () => scope.org,
  useDocumentTitle: vi.fn(),
}));
vi.mock("@/features/environment", () => ({ useEnvironment: () => scope.environment }));

const at = "2026-10-11T12:00:00.000Z";
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
const neighbor: Organization = { ...org, id: newId("organization"), name: "Cedar", slug: "cedar" };
const key: ApiKey = {
  id: newId("api_key"),
  organization_id: org.id,
  environment: "sandbox",
  name: "Queue worker",
  prefix: "zf_test_A1b2",
  last4: "7890",
  created_by: { id: newId("user"), name: "Jordan Lee", email: "jordan@fernhill.example", avatar_url: null },
  created_at: at,
  last_used_at: null,
  revoked_at: null,
};
// Deliberately not a usable bearer; assertions report booleans rather than values.
const secret = "api-key-one-time-fixture-only";
const created: CreatedApiKey = { key, secret };
const clients: QueryClient[] = [];

function setup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity }, mutations: { retry: false } },
  });
  clients.push(client);
  const Page = Route.options.component;
  const renderPage = () => <QueryClientProvider client={client}>{Page ? <Page /> : null}</QueryClientProvider>;
  const view = render(renderPage());
  return {
    ...view,
    client,
    change: (next: Partial<typeof scope>) => {
      Object.assign(scope, next);
      view.rerender(renderPage());
    },
  };
}

async function startCreate() {
  await userEvent.click(await screen.findByRole("button", { name: "Create Sandbox key" }));
  const dialog = await screen.findByRole("dialog", { name: "Create Sandbox key" });
  await userEvent.type(within(dialog).getByLabelText("Name"), key.name);
  await userEvent.click(within(dialog).getByRole("button", { name: "Create key" }));
}

function deferredCreate() {
  let resolve: (value: CreatedApiKey) => void = () => {
    throw new Error("The creation response must be initialized before it can finish.");
  };
  const promise = new Promise<CreatedApiKey>((finish) => {
    resolve = finish;
  });
  return { promise, resolve };
}

const secretVisible = () => document.body.textContent?.includes(secret) ?? false;
const expectReveal = () => waitFor(() => expect(secretVisible()).toBe(true), { onTimeout: (error) => error });
const expectNoSecretInCaches = (client: QueryClient) => {
  const data = [
    ...client
      .getQueryCache()
      .getAll()
      .map((query) => query.state.data),
    ...client
      .getMutationCache()
      .getAll()
      .map((mutation) => mutation.state.data),
  ];
  expect(JSON.stringify(data).includes(secret)).toBe(false);
};
type ScopeChange = "organization" | "environment" | "permission";
const changes: Record<ScopeChange, Partial<typeof scope>> = {
  organization: { org: neighbor },
  environment: { environment: "production" },
  permission: { org: { ...org, role: "viewer" } },
};

describe("API-key one-time secrets", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    scope.org = org;
    scope.environment = "sandbox";
    api.apiKeys.list.mockImplementation(async (organizationId) => [{ ...key, organization_id: organizationId }]);
    api.apiKeys.create.mockResolvedValue(created);
    api.apiKeys.revoke.mockResolvedValue({ ...key, revoked_at: at });
  });
  afterEach(() => {
    cleanup();
    for (const client of clients.splice(0)) client.clear();
  });

  it("keeps a mounted mutation's result and retained cache data public", async () => {
    const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
    clients.push(client);
    const reveal = vi.fn();
    const view = renderHook(() => useCreateApiKey(org.id, reveal), {
      wrapper: ({ children }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>,
    });
    await act(async () => {
      const result = await view.result.current.mutateAsync({ name: key.name, environment: "sandbox" });
      expect(JSON.stringify(result) === JSON.stringify(key)).toBe(true);
    });
    await waitFor(() => expect(view.result.current.isSuccess).toBe(true));
    expect(reveal.mock.calls.length).toBe(1);
    expect(reveal.mock.calls[0]?.[0]?.secret === secret).toBe(true);
    // The observer stays mounted, so gcTime cannot hide a secret-bearing result.
    const mutations = client.getMutationCache().getAll();
    expect(mutations.length).toBe(1);
    expect(JSON.stringify(view.result.current.data) === JSON.stringify(key)).toBe(true);
    expect(JSON.stringify(mutations[0]?.state.data) === JSON.stringify(key)).toBe(true);
    expectNoSecretInCaches(client);
  });

  it("reveals once, refreshes public metadata, and keeps the secret out of query and mutation data", async () => {
    const { client } = setup();
    client.setQueryData(qk.apiKeys(neighbor.id), []);
    client.setQueryData(qk.viewer, { organizations: [org] });
    await startCreate();
    await expectReveal();
    expect(api.apiKeys.create).toHaveBeenCalledExactlyOnceWith(org.id, { name: key.name, environment: "sandbox" });
    await waitFor(() => expect(api.apiKeys.list).toHaveBeenCalledTimes(2));
    expect(client.getQueryState(qk.viewer)?.isInvalidated).toBe(true);
    expect(client.getQueryState(qk.apiKeys(neighbor.id))?.isInvalidated).toBe(false);
    expectNoSecretInCaches(client);
    await userEvent.click(screen.getByRole("button", { name: "I've stored it" }));
    expect(secretVisible()).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Create Sandbox key" }));
    expect(secretVisible()).toBe(false);
    expect(screen.getByLabelText("Name")).toHaveValue("");
    expectNoSecretInCaches(client);
  });

  it.each(["organization", "environment", "permission"] as const)(
    "discards the reveal after a change of %s and does not restore it on return",
    async (change) => {
      const view = setup();
      await startCreate();
      await expectReveal();
      view.change(changes[change]);
      expect(secretVisible()).toBe(false);
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      view.change({ org, environment: "sandbox" });
      expect(secretVisible()).toBe(false);
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      expectNoSecretInCaches(view.client);
    },
  );

  it.each(["organization", "environment", "permission"] as const)(
    "discards open create and revoke dialogs after a change of %s",
    async (change) => {
      const view = setup();
      await userEvent.click(await screen.findByRole("button", { name: "Create Sandbox key" }));
      view.change(changes[change]);
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      view.change({ org, environment: "sandbox" });
      await userEvent.click(await screen.findByRole("button", { name: "Revoke" }));
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      view.change(changes[change]);
      expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
      view.change({ org, environment: "sandbox" });
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
      expect(api.apiKeys.create).not.toHaveBeenCalled();
      expect(api.apiKeys.revoke).not.toHaveBeenCalled();
    },
  );

  it.each(["organization", "environment", "permission"] as const)(
    "does not reveal a late creation response after a change of %s, even after returning",
    async (change) => {
      const response = deferredCreate();
      api.apiKeys.create.mockReturnValue(response.promise);
      const view = setup();
      await startCreate();
      expect(api.apiKeys.create).toHaveBeenCalledTimes(1);
      view.change(changes[change]);
      view.change({ org, environment: "sandbox" });
      // A new form in the original scope must not receive the old form's response.
      await userEvent.click(await screen.findByRole("button", { name: "Create Sandbox key" }));
      await act(async () => response.resolve(created));
      expect(secretVisible()).toBe(false);
      expect(screen.getByRole("dialog", { name: "Create Sandbox key" })).toBeInTheDocument();
      expectNoSecretInCaches(view.client);
    },
  );

  it("does not reopen a dismissed create form when its response arrives late", async () => {
    const response = deferredCreate();
    api.apiKeys.create.mockReturnValue(response.promise);
    const { client } = setup();
    await startCreate();
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Cancel" }));
    await userEvent.click(screen.getByRole("button", { name: "Create Sandbox key" }));
    await act(async () => response.resolve(created));
    expect(secretVisible()).toBe(false);
    expect(screen.getByRole("dialog", { name: "Create Sandbox key" })).toBeInTheDocument();
    expectNoSecretInCaches(client);
  });
});
