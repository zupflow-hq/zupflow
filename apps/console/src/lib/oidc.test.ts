import { describe, expect, it } from "vitest";
import {
  authorizationUrl,
  challengeOf,
  checkIdToken,
  codeFromCallback,
  discover,
  exchangeCode,
  isAllowedEndpoint,
  newPendingSignIn,
  PENDING_MAX_AGE_MS,
  type ProviderMetadata,
  randomToken,
  refreshTokens,
  SignInError,
  safeReturnPath,
} from "./oidc";

const ISSUER = "https://id.example";
const CLIENT = "zupflow-console";
const NOW = Date.parse("2026-10-10T12:00:00Z");
const provider: ProviderMetadata = {
  issuer: ISSUER,
  authorization_endpoint: "https://id.example/authorize",
  token_endpoint: "https://id.example/token",
};

function b64url(value: unknown) {
  return btoa(JSON.stringify(value)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** An unsigned token with these claims: the Console never checks signatures, the API does. */
function idToken(claims: Record<string, unknown>) {
  return `${b64url({ alg: "RS256", typ: "JWT" })}.${b64url({
    iss: ISSUER,
    aud: CLIENT,
    sub: "person-1",
    exp: NOW / 1000 + 300,
    ...claims,
  })}.signature`;
}

async function failureOf(run: () => unknown): Promise<SignInError> {
  try {
    await run();
  } catch (error) {
    if (error instanceof SignInError) return error;
    throw error;
  }
  throw new Error("Expected the sign-in to fail");
}

function answering(status: number, body: unknown) {
  const sent: { url: string; init: RequestInit | undefined }[] = [];
  const fetchFn: typeof fetch = async (input, init) => {
    sent.push({ url: String(input), init });
    return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
  };
  return { fetchFn, sent };
}

describe("PKCE", () => {
  it("derives the S256 challenge of RFC 7636's example", async () => {
    expect(await challengeOf("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")).toBe(
      "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
    );
  });

  it("makes fresh, URL-safe tokens of 256 bits", () => {
    const tokens = new Set(Array.from({ length: 50 }, () => randomToken()));
    expect(tokens.size).toBe(50);
    for (const token of tokens) expect(token).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });
});

describe("starting a sign-in", () => {
  it("asks for a code with PKCE, a state and a nonce, at the registered redirect URI", async () => {
    const pending = newPendingSignIn("/acme", "https://console.example/sign-in/callback", NOW);
    const url = new URL(await authorizationUrl(provider, CLIENT, pending));
    expect(url.origin + url.pathname).toBe(provider.authorization_endpoint);
    expect(Object.fromEntries(url.searchParams)).toEqual({
      response_type: "code",
      client_id: CLIENT,
      redirect_uri: "https://console.example/sign-in/callback",
      scope: "openid email profile",
      state: pending.state,
      nonce: pending.nonce,
      code_challenge: await challengeOf(pending.verifier),
      code_challenge_method: "S256",
    });
    expect(url.searchParams.get("code_challenge")).not.toBe(pending.verifier);
    expect(new Set([pending.state, pending.nonce, pending.verifier]).size).toBe(3);
  });

  it("reads the provider's endpoints only from its own issuer, over HTTPS", async () => {
    const document = {
      issuer: ISSUER,
      authorization_endpoint: provider.authorization_endpoint,
      token_endpoint: provider.token_endpoint,
    };
    const { fetchFn, sent } = answering(200, document);
    expect(await discover(ISSUER, fetchFn)).toEqual(provider);
    expect(sent[0]?.url).toBe("https://id.example/.well-known/openid-configuration");

    for (const [status, body] of [
      [200, { ...document, issuer: "https://id.example/" }],
      [200, { ...document, issuer: "https://attacker.example" }],
      [200, { ...document, token_endpoint: "http://id.example/token" }],
      [200, { ...document, authorization_endpoint: "javascript:alert(1)" }],
      [200, { issuer: ISSUER }],
      [404, document],
    ] as const) {
      expect((await failureOf(() => discover(ISSUER, answering(status, body).fetchFn))).code).toBe("discovery_failed");
    }
  });

  it("allows plain HTTP only on loopback", () => {
    expect(isAllowedEndpoint("https://id.example/token")).toBe(true);
    expect(isAllowedEndpoint("http://localhost:8180/token")).toBe(true);
    expect(isAllowedEndpoint("http://127.0.0.1:8180/token")).toBe(true);
    expect(isAllowedEndpoint("http://id.example/token")).toBe(false);
    expect(isAllowedEndpoint("https://user:pass@id.example/token")).toBe(false);
    expect(isAllowedEndpoint("not a url")).toBe(false);
    expect(isAllowedEndpoint(undefined)).toBe(false);
  });
});

describe("finishing a sign-in", () => {
  const pending = newPendingSignIn("/acme", "https://console.example/sign-in/callback", NOW);
  const callback = (params: Record<string, string>) =>
    new URL(`https://console.example/sign-in/callback?${new URLSearchParams(params)}`);

  it("takes the code only for the state this tab started, once, and not too late", () => {
    expect(codeFromCallback(callback({ code: "c1", state: pending.state }), pending, NOW)).toEqual({
      code: "c1",
      pending,
    });
    for (const [url, stored, now, code] of [
      [callback({ code: "c1", state: "forged" }), pending, NOW, "state_mismatch"],
      [callback({ code: "c1" }), pending, NOW, "state_mismatch"],
      [callback({ code: "c1", state: pending.state }), null, NOW, "state_mismatch"],
      [callback({ code: "c1", state: pending.state }), pending, NOW + PENDING_MAX_AGE_MS + 1, "sign_in_expired"],
      [callback({ error: "access_denied", state: pending.state }), pending, NOW, "consent_denied"],
      [
        callback({ error: "server_error", error_description: "<b>hi</b>", state: pending.state }),
        pending,
        NOW,
        "provider_refused",
      ],
      [callback({ state: pending.state }), pending, NOW, "provider_refused"],
    ] as const) {
      let failure: unknown;
      try {
        codeFromCallback(url, stored, now);
      } catch (error) {
        failure = error;
      }
      expect(failure).toBeInstanceOf(SignInError);
      expect((failure as SignInError).code, url.search).toBe(code);
      // The provider's own words are never shown.
      expect((failure as SignInError).message).not.toContain("<b>");
    }
  });

  it("exchanges the code with its verifier, and keeps only a token for this sign-in", async () => {
    const token = idToken({ nonce: pending.nonce });
    const { fetchFn, sent } = answering(200, { id_token: token, refresh_token: "r1", access_token: "a1" });
    expect(await exchangeCode(provider, CLIENT, pending, "c1", fetchFn, NOW)).toEqual({
      idToken: token,
      expiresAt: NOW + 300_000,
      subject: "person-1",
      refreshToken: "r1",
    });
    expect(sent[0]?.url).toBe(provider.token_endpoint);
    expect(sent[0]?.init?.method).toBe("POST");
    expect(Object.fromEntries(new URLSearchParams(String(sent[0]?.init?.body)))).toEqual({
      grant_type: "authorization_code",
      code: "c1",
      redirect_uri: pending.redirectUri,
      client_id: CLIENT,
      code_verifier: pending.verifier,
    });

    for (const claims of [
      { nonce: "another sign-in's" },
      {},
      { nonce: pending.nonce, iss: "https://attacker.example" },
      { nonce: pending.nonce, aud: "another-client" },
      { nonce: pending.nonce, sub: "" },
      { nonce: pending.nonce, exp: "tomorrow" },
    ]) {
      const answer = answering(200, { id_token: idToken(claims) });
      expect((await failureOf(() => exchangeCode(provider, CLIENT, pending, "c1", answer.fetchFn, NOW))).code).toBe(
        "invalid_id_token",
      );
    }
    for (const [status, body] of [
      [400, { error: "invalid_grant" }],
      [200, { access_token: "a1" }],
    ] as const) {
      const answer = answering(status, body);
      expect((await failureOf(() => exchangeCode(provider, CLIENT, pending, "c1", answer.fetchFn, NOW))).code).toBe(
        "token_exchange_failed",
      );
    }
  });

  it("takes an audience list only when the Console is its authorized party", () => {
    const expected = { issuer: ISSUER, clientId: CLIENT, nonce: "n" };
    expect(checkIdToken(idToken({ aud: ["api", CLIENT], azp: CLIENT, nonce: "n" }), expected, NOW)).toEqual({
      expiresAt: NOW + 300_000,
      subject: "person-1",
    });
    for (const claims of [
      { aud: ["api", CLIENT], nonce: "n" },
      { aud: ["api", CLIENT], azp: "api", nonce: "n" },
    ]) {
      expect(() => checkIdToken(idToken(claims), expected, NOW)).toThrow(SignInError);
    }
    expect(checkIdToken(idToken({ aud: [CLIENT], nonce: "n" }), expected, NOW).subject).toBe("person-1");
    expect(() => checkIdToken("not.a-token", { issuer: ISSUER, clientId: CLIENT, nonce: null }, NOW)).toThrow(
      SignInError,
    );
  });

  it("allows a minute of clock skew, and blames the clock, not the configuration, past it", async () => {
    const expected = { issuer: ISSUER, clientId: CLIENT, nonce: null };
    expect(checkIdToken(idToken({ exp: NOW / 1000 - 30 }), expected, NOW).expiresAt).toBe(NOW - 30_000);
    const late = await failureOf(() => checkIdToken(idToken({ exp: NOW / 1000 - 61 }), expected, NOW));
    expect(late.code).toBe("id_token_expired");
    expect(late.message).not.toContain("ZUPFLOW_OIDC_AUDIENCE");
    expect(late.message).toContain("clock");
  });

  it("refreshes with the refresh token, for the same person, keeping it when the provider doesn't rotate it", async () => {
    const token = idToken({});
    const { fetchFn, sent } = answering(200, { id_token: token });
    const previous = { refreshToken: "r1", subject: "person-1" };
    expect(await refreshTokens(provider, CLIENT, previous, fetchFn, NOW)).toEqual({
      idToken: token,
      expiresAt: NOW + 300_000,
      subject: "person-1",
      refreshToken: "r1",
    });
    const someoneElse = answering(200, { id_token: idToken({ sub: "person-2" }) });
    expect((await failureOf(() => refreshTokens(provider, CLIENT, previous, someoneElse.fetchFn, NOW))).code).toBe(
      "invalid_id_token",
    );
    expect(Object.fromEntries(new URLSearchParams(String(sent[0]?.init?.body)))).toEqual({
      grant_type: "refresh_token",
      refresh_token: "r1",
      client_id: CLIENT,
    });
  });
});

describe("returning after sign-in", () => {
  it("goes only to a path in the Console", () => {
    expect(safeReturnPath("/acme/sandbox/workspaces?q=1")).toBe("/acme/sandbox/workspaces?q=1");
    expect(safeReturnPath("/acme#runs", "https://console.example")).toBe("/acme#runs");
    expect(safeReturnPath("/a/../b?x=1", "https://console.example")).toBe("/b?x=1");
    for (const unsafe of [
      "https://attacker.example",
      "//attacker.example",
      "/\\attacker.example",
      "/\t/attacker.example",
      "/\n/attacker.example",
      "\\attacker.example",
      "",
      null,
      undefined,
    ]) {
      expect(safeReturnPath(unsafe)).toBe("/");
    }
  });
});
