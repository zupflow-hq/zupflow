import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type Invitation, type Organization, ZupflowApiError } from "@zupflow/sdk";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { InvitationList } from "./invitation-list";

const { api, navigate, toast } = vi.hoisted(() => ({
  api: { invitations: { list: vi.fn(), accept: vi.fn(), decline: vi.fn() } },
  navigate: vi.fn(),
  toast: Object.assign(vi.fn(), { success: vi.fn(), error: vi.fn(), info: vi.fn() }),
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("@tanstack/react-router", () => ({ useNavigate: () => navigate }));
vi.mock("sonner", () => ({ toast }));

const DAY = 86_400_000;
const invitation: Invitation = {
  id: "mem_0000000000000000000001" as Invitation["id"],
  organization: { id: "org_0000000000000000000001" as Organization["id"], name: "Fernhill", slug: "fernhill" },
  role: "developer",
  invited_by: { name: "Casey Morgan", email: "casey@fernhill.example" },
  status: "pending",
  invited_at: new Date(Date.now() - DAY).toISOString(),
  expires_at: new Date(Date.now() + 6 * DAY).toISOString(),
};

const apiError = (status: number, code: string, message = "Something happened.") =>
  new ZupflowApiError(status, { code, message, fields: null });

function renderList() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <InvitationList />
    </QueryClientProvider>,
  );
}

describe("InvitationList", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    api.invitations.list.mockResolvedValue([invitation]);
  });
  afterEach(cleanup);

  it("names who sent each invitation, with their address", async () => {
    renderList();
    expect(await screen.findByText(/Casey Morgan invited you as a developer/)).toBeInTheDocument();
    expect(screen.getByText("From casey@fernhill.example")).toBeInTheDocument();
  });

  it("says when invitations couldn't load, and offers a retry", async () => {
    api.invitations.list.mockRejectedValue(apiError(500, "internal", "The API is down."));
    renderList();
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load your invitations.");
    expect(screen.getByRole("button", { name: /retry/i })).toBeInTheDocument();
  });

  it("explains a sign-in that can't answer invitations, without a retry that can't help", async () => {
    const message = "Invitations go to jordan@northwind.example. Verify that address with your sign-in provider.";
    api.invitations.list.mockRejectedValue(apiError(403, "email_unverified", message));
    renderList();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Your sign-in can't answer invitations");
    expect(alert).toHaveTextContent(message);
    expect(screen.queryByRole("button", { name: /retry/i })).toBeNull();
  });

  it("joins with a fresh idempotency key per click, and goes into the organization", async () => {
    api.invitations.accept.mockResolvedValue({ name: "Fernhill", slug: "fernhill" });
    renderList();
    await userEvent.click(await screen.findByRole("button", { name: "Accept and join Fernhill" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: "/$org", params: { org: "fernhill" } }));
    expect(api.invitations.accept).toHaveBeenCalledWith(invitation.id, {
      idempotencyKey: expect.stringMatching(/^[0-9a-f]{32}$/),
    });
    await userEvent.click(screen.getByRole("button", { name: "Accept and join Fernhill" }));
    await waitFor(() => expect(api.invitations.accept).toHaveBeenCalledTimes(2));
    const [first, second] = api.invitations.accept.mock.calls.map(([, options]) => options.idempotencyKey);
    expect(first).not.toBe(second);
  });

  it("goes into the organization when the invitation was already accepted", async () => {
    api.invitations.accept.mockRejectedValue(apiError(409, "invitation_accepted"));
    renderList();
    await userEvent.click(await screen.findByRole("button", { name: "Accept and join Fernhill" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: "/$org", params: { org: "fernhill" } }));
    expect(toast.info).toHaveBeenCalledWith("You're already a member of Fernhill");
    expect(toast.error).not.toHaveBeenCalled();
  });

  it("says a withdrawn invitation was withdrawn, and goes nowhere", async () => {
    api.invitations.accept.mockRejectedValue(apiError(404, "not_found", "Invitation not found."));
    renderList();
    await userEvent.click(await screen.findByRole("button", { name: "Accept and join Fernhill" }));
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith("The invitation to Fernhill was withdrawn", expect.anything()),
    );
    expect(navigate).not.toHaveBeenCalled();
    // And the list is read again, so it's gone from there too.
    await waitFor(() => expect(api.invitations.list).toHaveBeenCalledTimes(2));
  });

  it("declines only once the person confirms", async () => {
    api.invitations.decline.mockResolvedValue(undefined);
    renderList();
    await userEvent.click(await screen.findByRole("button", { name: "Decline the invitation to Fernhill" }));
    expect(await screen.findByRole("alertdialog")).toHaveTextContent("Decline the invitation to Fernhill?");
    expect(api.invitations.decline).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.invitations.decline).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Decline the invitation to Fernhill" }));
    await userEvent.click(await screen.findByRole("button", { name: "Decline" }));
    await waitFor(() => expect(api.invitations.decline).toHaveBeenCalledWith(invitation.id));
    expect(toast.success).toHaveBeenCalledWith("Declined the invitation to Fernhill");
  });
});
