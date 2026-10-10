import { cleanup, render, screen } from "@testing-library/react";
import type { ConnectSession } from "@zupflow/sdk";
import { afterEach, describe, expect, it } from "vitest";
import { hostedFailure } from "./failure";
import { SessionEnded } from "./session-ended";

const app = {
  id: "app_0000000000000000000001",
  name: "Fieldnote",
  slug: "fieldnote",
  owner_scope: "organization",
  icon: { glyph: "Fn", color: "#4D7C0F" },
  auth_type: "mcp_oauth",
} as ConnectSession["app"];

const ended = (failure_code: string | null, return_url: string | null = null) => ({
  id: "cs_0000000000000000000001" as ConnectSession["id"],
  app,
  status: "failed" as const,
  failure_code,
  return_url,
});

describe("hostedFailure", () => {
  it("sends the person to the app's support when starting again fails the same way", () => {
    for (const code of ["discovery_failed", "identity_off_server", "app_not_eligible"]) {
      const copy = hostedFailure(ended(code));
      expect(copy.next, code).toBe("contact");
      expect(copy.body, code).toContain("contact support for the app that sent you here");
    }
  });

  it("asks the person to start again when the next attempt can work", () => {
    for (const code of ["provider_unavailable", "token_exchange_failed", "consent_denied", "account_mismatch", null]) {
      expect(hostedFailure(ended(code)).next, String(code)).toBe("start_again");
    }
  });

  it("blames the server, not a client the person never saw, when sign-in through it is refused", () => {
    expect(hostedFailure(ended("token_exchange_failed")).body).toContain("Fieldnote's server didn't accept");
    const oauth = { ...ended("token_exchange_failed"), app: { ...app, auth_type: "oauth2" as const } };
    expect(hostedFailure(oauth).body).toMatch(/^Fieldnote didn't accept/);
  });
});

describe("SessionEnded", () => {
  afterEach(cleanup);

  it("offers nothing that would repeat a sign-in the server can't offer", () => {
    render(<SessionEnded session={ended("discovery_failed")} />);
    expect(screen.getByRole("heading")).toHaveTextContent("Fieldnote can't be connected yet");
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("goes back to the app with the session named, so it can start again", () => {
    render(<SessionEnded session={ended("provider_unavailable", "https://app.northwind.example/connected?x=1")} />);
    expect(screen.getByRole("heading")).toHaveTextContent("Fieldnote couldn't be reached");
    expect(screen.getByRole("link", { name: "Go back" })).toHaveAttribute(
      "href",
      "https://app.northwind.example/connected?x=1&zupflow_session=cs_0000000000000000000001",
    );
  });

  it("closes a window another opened", () => {
    render(<SessionEnded session={ended("consent_denied")} canClose />);
    expect(screen.getByRole("button", { name: "Close window" })).toBeInTheDocument();
  });
});
