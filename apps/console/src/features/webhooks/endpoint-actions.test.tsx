import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type Organization, type WebhookEndpoint, ZupflowApiError } from "@zupflow/sdk";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useWebhooks } from "@/features/queries";
import { qk } from "@/lib/query-keys";
import { newId } from "@/mocks/ids";
import { Route } from "@/routes/_authed/$org/webhooks/index";
import { WebhookEndpointActions, WebhookSecretOverlap } from "./endpoint-actions";

const { api, toasts, edit, remove } = vi.hoisted(() => ({
  api: { webhooks: { list: vi.fn(), rollSecret: vi.fn(), sendTest: vi.fn() } },
  toasts: { success: vi.fn(), error: vi.fn() },
  edit: vi.fn(),
  remove: vi.fn(),
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: toasts }));
vi.mock("@/features/context", async (original) => ({
  ...(await original<typeof import("@/features/context")>()),
  useOrg: () => org,
  useDocumentTitle: vi.fn(),
}));
vi.mock("@/features/environment", () => ({ useEnvironment: () => "sandbox" }));
vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  Link: ({ children }: { children: React.ReactNode }) => <span>{children}</span>,
}));

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
const endpoint: WebhookEndpoint = {
  id: newId("webhook_endpoint"),
  organization_id: org.id,
  environment: "sandbox",
  url: "https://hooks.fernhill.example/events",
  description: "Run events",
  events: ["run.completed"],
  enabled: true,
  signing_secret_last4: "1234",
  previous_signing_secret_expires_at: null,
  created_at: at,
  last_delivery: null,
};
const secret = "whsec_rotation_fixture_9876";
const rotated: WebhookEndpoint = {
  ...endpoint,
  signing_secret_last4: "9876",
  previous_signing_secret_expires_at: "2026-10-12T12:00:00.000Z",
};
const conflict = () =>
  new ZupflowApiError(409, {
    code: "rotation_in_progress",
    message: "The previous secret is still active.",
    fields: null,
  });

function Harness({
  currentOrg = org,
  environment = endpoint.environment,
}: {
  currentOrg?: Organization;
  environment?: WebhookEndpoint["environment"];
}) {
  const hooks = useWebhooks(currentOrg.id);
  const current = hooks.data?.find((hook) => hook.environment === environment);
  return current ? (
    <>
      <p>Secret ending {current.signing_secret_last4}</p>
      <WebhookSecretOverlap endpoint={current} />
      <WebhookEndpointActions org={currentOrg} endpoint={current} onEdit={edit} onDelete={remove} />
    </>
  ) : null;
}

const clients: QueryClient[] = [];
function setup(props: Parameters<typeof Harness>[0] = {}, fullPage = false) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity }, mutations: { retry: false } },
  });
  clients.push(client);
  const Page = Route.options.component;
  const view = render(
    <QueryClientProvider client={client}>{fullPage && Page ? <Page /> : <Harness {...props} />}</QueryClientProvider>,
  );
  return {
    ...view,
    client,
    change: (next: Parameters<typeof Harness>[0]) =>
      view.rerender(
        <QueryClientProvider client={client}>
          <Harness {...next} />
        </QueryClientProvider>,
      ),
  };
}

async function openMenu() {
  (await screen.findByRole("button", { name: "Endpoint actions" })).focus();
  await userEvent.keyboard("{Enter}");
  return screen.findByRole("menu");
}
async function confirmRotation() {
  const menu = await openMenu();
  await userEvent.click(within(menu).getByRole("menuitem", { name: "Rotate signing secret" }));
  const dialog = await screen.findByRole("alertdialog");
  await userEvent.click(within(dialog).getByRole("button", { name: "Rotate secret" }));
}

describe("webhook signing-secret rotation", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    api.webhooks.list.mockResolvedValue([endpoint]);
    api.webhooks.rollSecret.mockImplementation(async () => {
      api.webhooks.list.mockResolvedValue([rotated]);
      return { endpoint: rotated, signing_secret: secret };
    });
    api.webhooks.sendTest.mockResolvedValue({ status_code: 204, delivered_at: at });
  });
  afterEach(() => {
    cleanup();
    for (const client of clients.splice(0)) client.clear();
    vi.useRealTimers();
  });

  it.each(["viewer", "developer"] as const)("hides management from a %s while showing the overlap", async (role) => {
    api.webhooks.list.mockResolvedValue([rotated]);
    setup({ currentOrg: { ...org, role } });
    expect(await screen.findByText(/Deliveries also use the previous secret/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Endpoint actions" })).not.toBeInTheDocument();
  });

  it("names the endpoint and environment, and cancelling makes no request", async () => {
    setup();
    await userEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Rotate signing secret" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText(endpoint.url)).toBeInTheDocument();
    expect(within(dialog).getByText(/· Sandbox/)).toBeInTheDocument();
    expect(within(dialog).getByText(/both secrets for 24 hours/)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(api.webhooks.rollSecret).not.toHaveBeenCalled();
  });

  it("reveals once, refreshes metadata and audit, and keeps the secret out of query and mutation data", async () => {
    const { client } = setup();
    const neighbor = { ...org, id: newId("organization") };
    client.setQueryData(qk.webhooks(neighbor.id), []);
    client.setQueryData(qk.audit(org.id), { items: [] });
    await confirmRotation();
    expect(await screen.findByText(secret)).toBeInTheDocument();
    expect(api.webhooks.rollSecret).toHaveBeenCalledExactlyOnceWith(org.id, endpoint.id);
    await waitFor(() => expect(screen.getByText("Secret ending 9876")).toBeInTheDocument());
    expect(client.getQueryState(qk.audit(org.id))?.isInvalidated).toBe(true);
    expect(client.getQueryState(qk.webhooks(neighbor.id))?.isInvalidated).toBe(false);
    expect(
      JSON.stringify(
        client
          .getQueryCache()
          .getAll()
          .map((query) => query.state.data),
      ),
    ).not.toContain(secret);
    expect(
      JSON.stringify(
        client
          .getMutationCache()
          .getAll()
          .map((mutation) => mutation.state.data),
      ),
    ).not.toContain(secret);
    await userEvent.click(screen.getByRole("button", { name: "I've stored it" }));
    expect(screen.queryByText(secret)).not.toBeInTheDocument();
    expect(screen.getByText(/You can rotate again after that/)).toBeInTheDocument();
    expect(within(await openMenu()).getByRole("menuitem", { name: "Rotate signing secret" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("allows an admin to rotate a disabled Production endpoint", async () => {
    const production = { ...endpoint, environment: "production" as const, enabled: false };
    api.webhooks.list.mockResolvedValue([production]);
    api.webhooks.rollSecret.mockImplementation(async () => {
      const updated = {
        ...production,
        signing_secret_last4: rotated.signing_secret_last4,
        previous_signing_secret_expires_at: rotated.previous_signing_secret_expires_at,
      };
      api.webhooks.list.mockResolvedValue([updated]);
      return { endpoint: updated, signing_secret: secret };
    });
    setup({ currentOrg: { ...org, role: "admin" }, environment: "production" });
    await userEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Rotate signing secret" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText(endpoint.url)).toBeInTheDocument();
    expect(within(dialog).getByText(/· Production/)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Rotate secret" }));
    expect(await screen.findByText(secret)).toBeInTheDocument();
  });

  it("keeps the one-time reveal open when refreshing the endpoint list fails", async () => {
    api.webhooks.rollSecret.mockImplementation(async () => {
      api.webhooks.list.mockRejectedValue(
        new ZupflowApiError(503, { code: "unavailable", message: "Try again shortly.", fields: null }),
      );
      return { endpoint: rotated, signing_secret: secret };
    });
    setup({}, true);
    await confirmRotation();
    expect(await screen.findByText("Try again shortly.")).toBeInTheDocument();
    expect(screen.getByText(secret)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "I've stored it" }));
    expect(screen.queryByText(secret)).not.toBeInTheDocument();
    expect(screen.getByText("9876")).toBeInTheDocument();
    expect(screen.getByText(/You can rotate again after that/)).toBeInTheDocument();
  });

  it("locks confirmation and cancellation while a roll is pending", async () => {
    let finish!: (value: { endpoint: WebhookEndpoint; signing_secret: string }) => void;
    api.webhooks.rollSecret.mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    setup();
    await confirmRotation();
    const dialog = screen.getByRole("alertdialog");
    expect(within(dialog).getByRole("button", { name: /Rotate secret/ })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeDisabled();
    await userEvent.keyboard("{Escape}");
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    await act(async () => finish({ endpoint: rotated, signing_secret: secret }));
    expect(api.webhooks.rollSecret).toHaveBeenCalledTimes(1);
    expect(await screen.findByText(secret)).toBeInTheDocument();
  });

  it("leaves a failed confirmation retryable without displaying a secret", async () => {
    api.webhooks.rollSecret.mockRejectedValueOnce(
      new ZupflowApiError(503, { code: "unavailable", message: "Try again shortly.", fields: null }),
    );
    setup();
    await confirmRotation();
    await waitFor(() => expect(toasts.error).toHaveBeenCalledWith("Try again shortly.", undefined));
    expect(screen.queryByText(secret)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Rotate secret" }));
    expect(await screen.findByText(secret)).toBeInTheDocument();
    expect(api.webhooks.rollSecret).toHaveBeenCalledTimes(2);
  });

  it("refreshes a stale endpoint after another manager wins the roll", async () => {
    api.webhooks.rollSecret.mockImplementation(async () => {
      api.webhooks.list.mockResolvedValue([rotated]);
      throw conflict();
    });
    setup();
    await confirmRotation();
    await waitFor(() => expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument());
    expect(await screen.findByText(/Deliveries also use the previous secret/)).toBeInTheDocument();
    expect(screen.queryByText(secret)).not.toBeInTheDocument();
    expect(within(await openMenu()).getByRole("menuitem", { name: "Rotate signing secret" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it.each(["role", "organization", "environment"] as const)(
    "discards the revealed secret after a %s switch",
    async (scope) => {
      const view = setup();
      await confirmRotation();
      expect(await screen.findByText(secret)).toBeInTheDocument();
      if (scope === "role") view.change({ currentOrg: { ...org, role: "viewer" } });
      if (scope === "organization") {
        const neighbor = { ...org, id: newId("organization"), slug: "cedar" };
        api.webhooks.list.mockResolvedValue([{ ...endpoint, organization_id: neighbor.id }]);
        view.change({ currentOrg: neighbor });
      }
      if (scope === "environment") view.change({ environment: "production" });
      await waitFor(() => expect(screen.queryByText(secret)).not.toBeInTheDocument());
    },
  );

  it("refetches at the server deadline and enables rotation when the overlap ends", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(at);
    const expiry = new Date(Date.parse(at) + 5_000).toISOString();
    api.webhooks.list.mockResolvedValue([{ ...rotated, previous_signing_secret_expires_at: expiry }]);
    setup();
    expect(await screen.findByText(/Deliveries also use the previous secret/)).toBeInTheDocument();
    api.webhooks.list.mockResolvedValue([{ ...rotated, previous_signing_secret_expires_at: null }]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    await waitFor(() => expect(screen.queryByText(/Deliveries also use the previous secret/)).not.toBeInTheDocument());
    expect(api.webhooks.list).toHaveBeenCalledTimes(2);
    expect(within(await openMenu()).getByRole("menuitem", { name: "Rotate signing secret" })).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("preserves test, edit and delete actions", async () => {
    setup();
    await userEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Send test event" }));
    await waitFor(() => expect(toasts.success).toHaveBeenCalledWith("Test event delivered (204)"));
    expect(api.webhooks.sendTest).toHaveBeenCalledWith(org.id, endpoint.id);
    await userEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Edit" }));
    expect(edit).toHaveBeenCalledTimes(1);
    await userEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Delete" }));
    expect(remove).toHaveBeenCalledTimes(1);
  });
});
