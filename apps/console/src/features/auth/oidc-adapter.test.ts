import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { newPendingSignIn } from "@/lib/oidc";
import { createOidcAdapter, redirectUri } from "./oidc-adapter";

const ISSUER = "https://id.example";
const CLIENT = "zupflow-console";

function b64url(value: unknown) {
  return btoa(JSON.stringify(value)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function idToken(nonce: string, lifetimeSeconds = 3600) {
  const exp = Math.floor(Date.now() / 1000) + lifetimeSeconds;
  return `${b64url({ alg: "RS256" })}.${b64url({ iss: ISSUER, aud: CLIENT, sub: "p1", exp, nonce })}.sig`;
}

/** The provider: discovery and a token endpoint answering `tokens`, counting exchanges. */
function provider(tokens: (form: URLSearchParams) => Record<string, unknown>) {
  const exchanges: URLSearchParams[] = [];
  vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === `${ISSUER}/.well-known/openid-configuration`) {
      return Response.json({
        issuer: ISSUER,
        authorization_endpoint: `${ISSUER}/authorize`,
        token_endpoint: `${ISSUER}/token`,
      });
    }
    if (url === `${ISSUER}/token`) {
      const form = new URLSearchParams(String(init?.body));
      exchanges.push(form);
      return Response.json(tokens(form));
    }
    return new Response(null, { status: 404 });
  });
  return exchanges;
}

describe("the oidc adapter", () => {
  beforeEach(() => sessionStorage.clear());
  afterEach(() => {
    vi.unstubAllGlobals();
    sessionStorage.clear();
  });

  it("finishes a sign-in once, keeps the ID token for this tab, and forgets it on sign-out", async () => {
    const pending = newPendingSignIn("/acme", redirectUri());
    sessionStorage.setItem("zupflow.oidc.pending", JSON.stringify(pending));
    const token = idToken(pending.nonce);
    const exchanges = provider(() => ({ id_token: token }));
    const changed = vi.fn();
    const adapter = createOidcAdapter({ mode: "oidc", issuer: ISSUER, client_id: CLIENT }, changed);
    if (adapter.controls.mode !== "oidc") throw new Error("expected oidc");
    expect(adapter.isSignedIn()).toBe(false);

    const callback = new URL(`${redirectUri()}?code=c1&state=${pending.state}`);
    // React may run the callback page's effect twice: one exchange, one answer.
    const [first, second] = await Promise.all([
      adapter.controls.completeSignIn(callback),
      adapter.controls.completeSignIn(callback),
    ]);
    expect([first, second]).toEqual([
      { ok: true, returnTo: "/acme" },
      { ok: true, returnTo: "/acme" },
    ]);
    expect(exchanges).toHaveLength(1);
    expect(exchanges[0]?.get("code_verifier")).toBe(pending.verifier);
    expect(sessionStorage.getItem("zupflow.oidc.pending")).toBeNull();
    expect(adapter.isSignedIn()).toBe(true);
    expect(await adapter.token()).toBe(token);
    expect(changed).toHaveBeenCalledTimes(1);

    // A reload in the same tab is still signed in.
    const reloaded = createOidcAdapter({ mode: "oidc", issuer: ISSUER, client_id: CLIENT }, () => {});
    expect(reloaded.isSignedIn()).toBe(true);
    expect(await reloaded.token()).toBe(token);

    adapter.signOut();
    expect(adapter.isSignedIn()).toBe(false);
    expect(await adapter.token()).toBeNull();
    expect(sessionStorage.getItem("zupflow.oidc.session")).toBeNull();
  });

  it("refuses a callback this tab didn't start, sends no exchange, and remembers where to return", async () => {
    const exchanges = provider(() => ({ id_token: idToken("n") }));
    sessionStorage.setItem("zupflow.oidc.pending", JSON.stringify(newPendingSignIn("/acme/runs", redirectUri())));
    const adapter = createOidcAdapter({ mode: "oidc", issuer: ISSUER, client_id: CLIENT }, () => {});
    if (adapter.controls.mode !== "oidc") throw new Error("expected oidc");
    const completion = await adapter.controls.completeSignIn(new URL(`${redirectUri()}?code=c1&state=forged`));
    expect(completion).toEqual({
      ok: false,
      returnTo: "/acme/runs",
      message: expect.stringMatching(/wasn't started from this browser tab/),
    });
    expect(sessionStorage.getItem("zupflow.oidc.pending")).toBeNull();
    expect(exchanges).toHaveLength(0);
    expect(adapter.isSignedIn()).toBe(false);
  });

  it("sends no expired token, and refreshes one when the provider gave a refresh token", async () => {
    const pending = newPendingSignIn("/", redirectUri());
    sessionStorage.setItem("zupflow.oidc.pending", JSON.stringify(pending));
    const renewed = idToken("", 3600);
    // The first token expires within the margin, so the next request refreshes it.
    provider((form) =>
      form.get("grant_type") === "refresh_token"
        ? { id_token: renewed }
        : { id_token: idToken(pending.nonce, 30), refresh_token: "r1" },
    );
    const adapter = createOidcAdapter({ mode: "oidc", issuer: ISSUER, client_id: CLIENT }, () => {});
    if (adapter.controls.mode !== "oidc") throw new Error("expected oidc");
    expect((await adapter.controls.completeSignIn(new URL(`${redirectUri()}?code=c1&state=${pending.state}`))).ok).toBe(
      true,
    );
    expect(await adapter.token()).toBe(renewed);

    // A tab reloaded with only an expiring token, and no refresh token in memory, sends none.
    sessionStorage.setItem("zupflow.oidc.session", JSON.stringify({ idToken: "old", expiresAt: Date.now() + 10_000 }));
    const reloaded = createOidcAdapter({ mode: "oidc", issuer: ISSUER, client_id: CLIENT }, () => {});
    expect(reloaded.isSignedIn()).toBe(false);
    expect(await reloaded.token()).toBeNull();
  });
});
