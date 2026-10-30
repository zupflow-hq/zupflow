import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  type Organization,
  type WebhookDeliveryDetail,
  type WebhookDeliveryId,
  type WebhookDeliveryListQuery,
  type WebhookEndpoint,
  ZupflowApiError,
} from "@zupflow/sdk";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { qk } from "@/lib/query-keys";
import { newId } from "@/mocks/ids";
import { WebhookHistory } from "./history";

const { api, toasts } = vi.hoisted(() => ({
  api: {
    webhooks: { deliveries: vi.fn(), delivery: vi.fn(), deliveryAttempts: vi.fn(), resendDelivery: vi.fn() },
    workspaces: { list: vi.fn(), get: vi.fn() },
  },
  toasts: { success: vi.fn(), error: vi.fn() },
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: toasts }));

const at = "2026-10-11T12:00:00.000Z";
const org: Organization = {
  id: newId("organization"),
  name: "Fernhill",
  slug: "fernhill",
  role: "viewer",
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
const workspace = { id: newId("workspace"), external_id: "fern-account" };
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
const eventId = newId("webhook_event");
const delivery: WebhookDeliveryDetail = {
  id: newId("webhook_delivery"),
  organization_id: org.id,
  endpoint_id: endpoint.id,
  event_id: eventId,
  environment: "sandbox",
  event_type: "run.completed",
  event_created_at: at,
  workspace,
  state: "failed",
  attempts: 2,
  last_status_code: 503,
  last_error: "status",
  last_attempt_at: at,
  next_attempt_at: null,
  created_at: at,
  attempt_history_complete: true,
  event: {
    id: eventId,
    organization_id: org.id,
    environment: "sandbox",
    type: "run.completed",
    created_at: at,
    workspace,
    data: { object: "run", id: newId("run"), agent_id: newId("agent"), status: "completed" },
  },
};
const page = <T,>(items: T[], next_cursor: string | null = null, total = items.length) => ({
  items,
  pagination: { next_cursor, total },
});

function freshDelivery(): WebhookDeliveryDetail {
  return {
    ...delivery,
    id: newId("webhook_delivery"),
    state: "pending",
    attempts: 0,
    last_status_code: null,
    last_error: null,
    last_attempt_at: null,
    next_attempt_at: at,
  };
}

function Harness({
  currentOrg = org,
  currentEndpoint = endpoint,
  filters = {},
  onFiltersChange = vi.fn(),
}: {
  currentOrg?: Organization;
  currentEndpoint?: WebhookEndpoint;
  filters?: Omit<WebhookDeliveryListQuery, "cursor" | "limit">;
  onFiltersChange?: (filters: Omit<WebhookDeliveryListQuery, "cursor" | "limit">) => void;
}) {
  const [selected, onSelect] = useState<WebhookDeliveryId>();
  return (
    <TooltipProvider>
      <WebhookHistory
        org={currentOrg}
        endpoint={currentEndpoint}
        filters={filters}
        onFiltersChange={onFiltersChange}
        selected={selected}
        onSelect={onSelect}
      />
    </TooltipProvider>
  );
}

const clients: QueryClient[] = [];
function setup(props: Parameters<typeof Harness>[0] = {}) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity }, mutations: { retry: false } },
  });
  clients.push(client);
  const view = render(
    <QueryClientProvider client={client}>
      <Harness {...props} />
    </QueryClientProvider>,
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

describe("webhook history", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    api.webhooks.deliveries.mockResolvedValue(page([delivery]));
    api.webhooks.delivery.mockResolvedValue(delivery);
    api.webhooks.deliveryAttempts.mockResolvedValue(
      page([
        { number: 1, status_code: null, error_class: "timeout", attempted_at: at, duration_ms: null },
        { number: 2, status_code: 503, error_class: "status", attempted_at: at, duration_ms: 42 },
      ]),
    );
    api.workspaces.list.mockResolvedValue(page([]));
    api.workspaces.get.mockResolvedValue({ ...workspace, name: "Fern account" });
  });
  afterEach(() => {
    cleanup();
    for (const client of clients.splice(0)) client.clear();
  });

  it("lets a viewer open details and recorded attempts in the selected endpoint", async () => {
    setup();
    expect(api.webhooks.delivery).not.toHaveBeenCalled();
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    const dialog = await screen.findByRole("dialog");
    expect(await within(dialog).findByText("42 ms")).toBeInTheDocument();
    expect(within(dialog).getByText("Timed out")).toBeInTheDocument();
    expect(within(dialog).getByText("Duration not recorded")).toBeInTheDocument();
    expect(api.webhooks.delivery).toHaveBeenCalledWith(org.id, endpoint.id, delivery.id);
    expect(api.webhooks.deliveryAttempts).toHaveBeenCalledWith(org.id, endpoint.id, delivery.id, {
      limit: 25,
      cursor: undefined,
    });
    expect(within(dialog).getByText("None scheduled")).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("loads older deliveries and further attempts only when requested", async () => {
    const older = { ...delivery, id: newId("webhook_delivery"), event_type: "run.failed" };
    api.webhooks.deliveries.mockImplementation((_org, _endpoint, query) =>
      Promise.resolve(query.cursor ? page([older], null, 2) : page([delivery], "older", 2)),
    );
    api.webhooks.deliveryAttempts.mockImplementation((_org, _endpoint, _delivery, query) =>
      Promise.resolve(
        query.cursor
          ? page([{ number: 2, status_code: 503, error_class: "status", attempted_at: at, duration_ms: 42 }], null, 2)
          : page(
              [{ number: 1, status_code: null, error_class: "timeout", attempted_at: at, duration_ms: null }],
              "next-attempt",
              2,
            ),
      ),
    );
    setup();
    await userEvent.click(await screen.findByRole("button", { name: "Load older deliveries" }));
    expect(await screen.findByText("run.failed")).toBeInTheDocument();
    expect(api.webhooks.deliveries).toHaveBeenLastCalledWith(org.id, endpoint.id, { cursor: "older", limit: 25 });
    expect(screen.queryByRole("button", { name: "Load older deliveries" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: /View run.completed delivery/ }));
    await userEvent.click(await screen.findByRole("button", { name: "Load more attempts" }));
    expect(await screen.findByText("42 ms")).toBeInTheDocument();
    expect(api.webhooks.deliveryAttempts).toHaveBeenLastCalledWith(org.id, endpoint.id, delivery.id, {
      cursor: "next-attempt",
      limit: 25,
    });
  });

  it("fetches the final attempt when a pending delivery completes and stops polling", async () => {
    api.webhooks.delivery.mockResolvedValue({ ...delivery, state: "pending", attempts: 1 });
    api.webhooks.deliveryAttempts.mockResolvedValue(
      page([{ number: 1, status_code: 503, error_class: "status", attempted_at: at, duration_ms: 42 }]),
    );
    const { client } = setup();
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    expect(await screen.findByText("42 ms")).toBeInTheDocument();
    api.webhooks.deliveryAttempts.mockResolvedValue(
      page([
        { number: 1, status_code: 503, error_class: "status", attempted_at: at, duration_ms: 42 },
        { number: 2, status_code: 204, error_class: null, attempted_at: at, duration_ms: 17 },
      ]),
    );
    act(() => {
      client.setQueryData(qk.webhookDelivery(org.id, endpoint.id, endpoint.environment, delivery.id), {
        ...delivery,
        state: "succeeded",
        last_status_code: 204,
        last_error: null,
      });
    });
    expect(await screen.findByText("17 ms")).toBeInTheDocument();
  });

  it("groups the event types it filters by what each is about, memory's together", async () => {
    // Radix measures the list it opens; jsdom has no layout.
    Element.prototype.scrollIntoView ??= () => {};
    globalThis.ResizeObserver ??= class {
      observe() {}
      unobserve() {}
      disconnect() {}
    };
    const onFiltersChange = vi.fn();
    setup({ onFiltersChange });
    (await screen.findByRole("combobox", { name: "Event type" })).focus();
    await userEvent.keyboard("{Enter}");
    const list = await screen.findByRole("listbox");
    const named = (group: HTMLElement) =>
      document.getElementById(group.getAttribute("aria-labelledby") ?? "")?.textContent;
    expect(within(list).getAllByRole("group").map(named)).toEqual([
      "Installations",
      "Connect sessions",
      "Runs",
      "Apps",
      "Channels",
      "Memory",
      "Test events",
    ]);
    const memory = within(list).getByRole("group", { name: "Memory" });
    expect(
      within(memory)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["memory.saved", "memory.updated", "memory.forgotten"]);
    await userEvent.click(within(memory).getByRole("option", { name: "memory.forgotten" }));
    expect(onFiltersChange).toHaveBeenCalledWith({ event_type: "memory.forgotten" });
  });

  it("explains legacy gaps without inventing individual results", async () => {
    api.webhooks.delivery.mockResolvedValue({ ...delivery, attempt_history_complete: false });
    api.webhooks.deliveryAttempts.mockResolvedValue(page([]));
    setup();
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    expect(await screen.findByRole("note")).toHaveTextContent("Some older attempts have only a summary");
    expect(await screen.findByText("No individual attempts recorded.")).toBeInTheDocument();
    expect(screen.queryByText("42 ms")).toBeNull();
  });

  it("shows loading, an error with retry, then the empty state", async () => {
    let reject: (error: unknown) => void = () => {};
    api.webhooks.deliveries.mockImplementationOnce(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    setup();
    expect(screen.getByRole("status", { name: "Loading", busy: true })).toHaveAttribute("aria-busy", "true");
    reject(new ZupflowApiError(503, { code: "unavailable", message: "Try again shortly.", fields: null }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load delivery history.");
    api.webhooks.deliveries.mockResolvedValue(page([]));
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("No deliveries for these filters")).toBeInTheDocument();
  });

  it("does not reuse cached rows across organizations, endpoints, environments or workspace filters", async () => {
    const view = setup();
    await screen.findByRole("button", { name: /View run.completed delivery/ });
    api.webhooks.deliveries.mockResolvedValue(page([]));
    view.change({ filters: { workspace_id: workspace.id } });
    await waitFor(() =>
      expect(api.webhooks.deliveries).toHaveBeenLastCalledWith(org.id, endpoint.id, {
        workspace_id: workspace.id,
        limit: 25,
        cursor: undefined,
      }),
    );
    expect(await screen.findByText("No deliveries for these filters")).toBeInTheDocument();
    view.change({ currentEndpoint: { ...endpoint, environment: "production" } });
    await waitFor(() => expect(api.webhooks.deliveries).toHaveBeenCalledTimes(3));
    const siblingEndpoint = { ...endpoint, id: newId("webhook_endpoint") };
    view.change({ currentEndpoint: siblingEndpoint });
    await waitFor(() =>
      expect(api.webhooks.deliveries).toHaveBeenLastCalledWith(org.id, siblingEndpoint.id, {
        limit: 25,
        cursor: undefined,
      }),
    );
    const foreignOrg = { ...org, id: newId("organization") };
    view.change({ currentOrg: foreignOrg });
    await waitFor(() =>
      expect(api.webhooks.deliveries).toHaveBeenLastCalledWith(foreignOrg.id, endpoint.id, {
        limit: 25,
        cursor: undefined,
      }),
    );
    expect(screen.queryByRole("button", { name: /View run.completed delivery/ })).toBeNull();
  });

  it.each([
    ["owner", "sandbox"],
    ["admin", "production"],
  ] as const)("lets an %s confirm a resend in %s and opens its fresh history", async (role, environment) => {
    const source = { ...delivery, environment, event: { ...delivery.event, environment } };
    const fresh = { ...freshDelivery(), environment, event: source.event };
    api.webhooks.resendDelivery.mockImplementation(async () => {
      api.webhooks.deliveries.mockResolvedValue(page([fresh, source]));
      return fresh;
    });
    api.webhooks.deliveries.mockResolvedValue(page([source]));
    api.webhooks.delivery.mockImplementation(async (_org, _endpoint, id) => (id === fresh.id ? fresh : source));
    api.webhooks.deliveryAttempts.mockImplementation(async (_org, _endpoint, id) =>
      page(
        id === fresh.id
          ? []
          : [{ number: 2, status_code: 503, error_class: "status", attempted_at: at, duration_ms: 42 }],
      ),
    );
    setup({ currentOrg: { ...org, role }, currentEndpoint: { ...endpoint, environment } });
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    await userEvent.click(await screen.findByRole("button", { name: "Resend event" }));
    let confirmation = await screen.findByRole("alertdialog");
    expect(confirmation).toHaveTextContent(endpoint.url);
    expect(confirmation).toHaveTextContent("same event ID and body");
    await userEvent.click(within(confirmation).getByRole("button", { name: "Cancel" }));
    expect(api.webhooks.resendDelivery).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Resend event" }));
    confirmation = await screen.findByRole("alertdialog");
    await userEvent.click(within(confirmation).getByRole("button", { name: "Resend event" }));
    await waitFor(() => expect(api.webhooks.delivery).toHaveBeenLastCalledWith(org.id, endpoint.id, fresh.id));
    const detail = await screen.findByRole("dialog");
    expect(await within(detail).findByText("No attempt recorded")).toBeInTheDocument();
    expect(within(detail).getByText("0 recorded attempts")).toBeInTheDocument();
    expect(within(detail).getByRole("button", { name: "Resend event" })).toBeDisabled();
    expect(api.webhooks.resendDelivery).toHaveBeenCalledExactlyOnceWith(org.id, endpoint.id, delivery.id);
    expect(toasts.success).toHaveBeenCalledWith("Event queued for resend");
    await userEvent.click(within(detail).getByRole("button", { name: "Close" }));
    const rows = await screen.findAllByRole("button", { name: /View run.completed delivery/ });
    expect(rows).toHaveLength(2);
    expect(rows[1]).toHaveTextContent("Failed");
    expect(rows[1]).toHaveTextContent("2 attempts");
  });

  it.each(["viewer", "developer"] as const)("keeps resend unavailable to a %s", async (role) => {
    setup({ currentOrg: { ...org, role } });
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    await screen.findByText("42 ms");
    expect(screen.queryByRole("button", { name: "Resend event" })).toBeNull();
    expect(api.webhooks.resendDelivery).not.toHaveBeenCalled();
  });

  it.each([
    ["pending", "This delivery is still pending. Wait until it finishes before resending."],
    ["disabled", "Enable this webhook endpoint before resending."],
    ["unsubscribed", "Subscribe this endpoint to the event type before resending."],
  ] as const)("explains why a %s delivery cannot be resent", async (condition, explanation) => {
    if (condition === "pending") api.webhooks.delivery.mockResolvedValue({ ...delivery, state: "pending" });
    setup({
      currentOrg: { ...org, role: "admin" },
      currentEndpoint: {
        ...endpoint,
        enabled: condition !== "disabled",
        events: condition === "unsubscribed" ? ["run.failed"] : endpoint.events,
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    expect(await screen.findByText(explanation)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resend event" })).toBeDisabled();
    expect(api.webhooks.resendDelivery).not.toHaveBeenCalled();
  });

  it("allows a test event without an event subscription", async () => {
    api.webhooks.delivery.mockResolvedValue({
      ...delivery,
      event_type: "webhook.test",
      workspace: null,
      event: {
        ...delivery.event,
        type: "webhook.test",
        workspace: null,
        data: { object: "webhook_endpoint", id: endpoint.id },
      },
    });
    setup({ currentOrg: { ...org, role: "admin" } });
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    expect(await screen.findByRole("button", { name: "Resend event" })).toBeEnabled();
  });

  it("blocks duplicate clicks while sending and keeps a rejected resend open for retry", async () => {
    let reject: (error: unknown) => void = () => {};
    api.webhooks.resendDelivery.mockImplementationOnce(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    const fresh = freshDelivery();
    api.webhooks.resendDelivery.mockResolvedValueOnce(fresh);
    api.webhooks.delivery.mockImplementation(async (_org, _endpoint, id) => (id === fresh.id ? fresh : delivery));
    setup({ currentOrg: { ...org, role: "owner" } });
    await userEvent.click(await screen.findByRole("button", { name: /View run.completed delivery/ }));
    await userEvent.click(await screen.findByRole("button", { name: "Resend event" }));
    const confirmation = await screen.findByRole("alertdialog");
    const action = within(confirmation).getByRole("button", { name: "Resend event" });
    await userEvent.dblClick(action);
    expect(action).toBeDisabled();
    expect(within(confirmation).getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(api.webhooks.resendDelivery).toHaveBeenCalledTimes(1);
    reject(new ZupflowApiError(503, { code: "unavailable", message: "Try again shortly.", fields: null }));
    await waitFor(() => expect(action).toBeEnabled());
    expect(toasts.error).toHaveBeenCalledWith("Couldn't resend event", { description: "Try again shortly." });
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    expect(api.webhooks.delivery).toHaveBeenLastCalledWith(org.id, endpoint.id, delivery.id);
    await userEvent.click(action);
    await waitFor(() => expect(api.webhooks.delivery).toHaveBeenLastCalledWith(org.id, endpoint.id, fresh.id));
    expect(api.webhooks.resendDelivery).toHaveBeenCalledTimes(2);
  });
});
