import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ConnectTarget, ZupflowApiError } from "@zupflow/sdk";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { newId } from "@/mocks/ids";
import { ChannelConnectBody, type ChannelConnectSession } from "./channel-connect";
import { hostedFailure } from "./failure";

const { api } = vi.hoisted(() => ({
  api: { connect: { submitCredentials: vi.fn(), authorize: vi.fn(), cancel: vi.fn() } },
}));
vi.mock("@/lib/api", () => ({ api }));

const at = "2026-10-13T12:00:00.000Z";
const linkTeam: ConnectTarget = { kind: "channel_app_manager", provider: "slack", manager: null };
const installApp: ConnectTarget = {
  kind: "channel_app",
  provider: "slack",
  app: newId("channel_app"),
  connection: null,
};
const session = (target: ConnectTarget, label: string): ChannelConnectSession => ({
  id: newId("connect_session"),
  workspace_id: newId("workspace"),
  environment: "sandbox",
  app: null,
  target,
  intent: "create",
  installation_id: null,
  label,
  status: "pending",
  return_url: null,
  created_at: at,
  expires_at: at,
  completed_at: null,
  failure_code: null,
  auth: null,
  requested_scopes: [],
  expected_account: null,
});
const reference = "a".repeat(32);
// Deliberately not usable tokens.
const configurationToken = "xoxe.xoxp-fixture-only-3b1d";
const refreshToken = "xoxe-fixture-refresh-only-77a0";

const renderBody = (s: ChannelConnectSession) =>
  render(
    <QueryClientProvider
      client={new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })}
    >
      <ChannelConnectBody session={s} reference={reference} />
    </QueryClientProvider>,
  );

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

describe("hosted Connect for a channel", () => {
  it("links a team with both tokens, and shows a refused one on its field", async () => {
    api.connect.submitCredentials.mockRejectedValue(
      new ZupflowApiError(422, {
        code: "invalid_configuration_token",
        message: "Slack refused this configuration token.",
        fields: { "values.configuration_token": "Slack refused this token. Generate a new one and paste it again." },
      }),
    );
    renderBody(session(linkTeam, "Slack"));
    expect(screen.getByRole("heading", { name: "Link your Slack team" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Link team" }));
    expect(screen.getByText("Paste the configuration token.")).toBeVisible();
    expect(api.connect.submitCredentials).not.toHaveBeenCalled();

    await userEvent.type(screen.getByLabelText("Configuration token"), configurationToken);
    await userEvent.type(screen.getByLabelText("Refresh token"), refreshToken);
    await userEvent.click(screen.getByRole("button", { name: "Link team" }));
    await waitFor(() => expect(api.connect.submitCredentials).toHaveBeenCalledTimes(1));
    const [ref, body] = api.connect.submitCredentials.mock.calls[0] ?? [];
    expect(ref).toBe(reference);
    expect(body.values.configuration_token === configurationToken && body.values.refresh_token === refreshToken).toBe(
      true,
    );
    expect(await screen.findByText("Slack refused this token. Generate a new one and paste it again.")).toBeVisible();
    // A pending session keeps what was pasted, so one wrong token doesn't mean pasting both again.
    expect(screen.getByLabelText("Refresh token")).toHaveValue(refreshToken);
  });

  it("sends the person to Slack to allow one agent's app", async () => {
    api.connect.authorize.mockRejectedValue(
      new ZupflowApiError(422, {
        code: "app_not_eligible",
        message: "This app can't be installed any more.",
        fields: null,
      }),
    );
    renderBody(session(installApp, "Support agent"));
    expect(screen.getByRole("heading", { name: "Add Support agent to Slack" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Continue to Slack" }));
    // The developer's words aren't the team admin's: they get what they can do.
    expect(await screen.findByRole("alert")).toHaveTextContent("Contact support for the app that sent you here.");
    expect(screen.getByRole("button", { name: "Continue to Slack" })).toBeDisabled();
  });

  it("says what a refusal on Slack's page changed", () => {
    const denied = hostedFailure({
      app: null,
      target: installApp,
      label: "Support agent",
      failure_code: "consent_denied",
    });
    expect(denied.title).toBe("The app wasn't allowed");
    expect(denied.body).toMatch(/^Support agent wasn't added to Slack\. .*choose Allow on Slack\.$/);
    expect(denied.next).toBe("start_again");
    expect(hostedFailure({ app: null, target: linkTeam, failure_code: null }).body).toMatch(/^No team was linked\./);
  });
});
