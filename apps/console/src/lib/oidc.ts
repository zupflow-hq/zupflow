/**
 * OpenID Connect sign-in for the Console, as a public client: the
 * authorization code flow with PKCE (S256), a single-use `state` and a
 * `nonce`. The ID token it ends with is the API's bearer; the API verifies its
 * signature against the issuer's keys (decision 0028). The Console only checks
 * what binds the token to this sign-in: issuer, audience, nonce and expiry.
 *
 * Generic on purpose: no provider's SDK or name (decision 0020). A provider
 * works when it publishes discovery, accepts a client without a secret and
 * answers its token endpoint cross-origin.
 */

export interface ProviderMetadata {
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
}

/** What a sign-in carries across the redirect to the provider and back. */
export interface PendingSignIn {
  state: string;
  nonce: string;
  verifier: string;
  /** The Console path to land on afterwards. */
  returnTo: string;
  redirectUri: string;
  /** When it started, in milliseconds; an old one is refused. */
  startedAt: number;
}

/** The tokens a sign-in or a refresh ends with. */
export interface TokenSet {
  idToken: string;
  /** The ID token's `exp`, in milliseconds. */
  expiresAt: number;
  /** The ID token's `sub`: a refresh must answer for the same person. */
  subject: string;
  refreshToken: string | null;
}

export type SignInFailure =
  | "discovery_failed"
  | "provider_refused"
  | "consent_denied"
  | "state_mismatch"
  | "sign_in_expired"
  | "token_exchange_failed"
  | "invalid_id_token"
  | "id_token_expired";

export class SignInError extends Error {
  constructor(
    readonly code: SignInFailure,
    message: string,
  ) {
    super(message);
    this.name = "SignInError";
  }
}

/** How far this device's clock may be from the provider's. */
export const CLOCK_SKEW_MS = 60_000;
/** A sign-in left at the provider for longer than this is started again. */
export const PENDING_MAX_AGE_MS = 10 * 60_000;
/** The scopes the API needs in the ID token: who, and a verified email (decision 0027). */
export const SCOPES = "openid email profile";

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);

/** HTTPS, or plain HTTP on loopback only, as the API fetches the issuer's keys. */
export function isAllowedEndpoint(value: unknown): value is string {
  if (typeof value !== "string") return false;
  try {
    const url = new URL(value);
    if (url.username || url.password) return false;
    return url.protocol === "https:" || (url.protocol === "http:" && LOOPBACK.has(url.hostname));
  } catch {
    return false;
  }
}

/** Reads the provider's discovery document; its `issuer` must be exactly the configured one. */
export async function discover(issuer: string, fetchFn: typeof fetch = fetch): Promise<ProviderMetadata> {
  const unusable = () =>
    new SignInError(
      "discovery_failed",
      `The identity provider at ${issuer} couldn't be read. Check that ZUPFLOW_OIDC_ISSUER is its issuer URL and that the provider is reachable from this browser.`,
    );
  let document: Record<string, unknown>;
  try {
    const response = await fetchFn(`${issuer.replace(/\/$/, "")}/.well-known/openid-configuration`, {
      headers: { Accept: "application/json" },
      redirect: "error",
    });
    if (!response.ok) throw unusable();
    document = (await response.json()) as Record<string, unknown>;
  } catch {
    throw unusable();
  }
  if (
    document.issuer !== issuer ||
    !isAllowedEndpoint(document.authorization_endpoint) ||
    !isAllowedEndpoint(document.token_endpoint)
  ) {
    throw unusable();
  }
  return {
    issuer,
    authorization_endpoint: document.authorization_endpoint,
    token_endpoint: document.token_endpoint,
  };
}

function base64url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** A random token of `bytes` bytes, base64url: 32 bytes make a 43-character PKCE verifier. */
export function randomToken(bytes = 32): string {
  return base64url(crypto.getRandomValues(new Uint8Array(bytes)));
}

/** The PKCE S256 challenge of a verifier (RFC 7636). */
export async function challengeOf(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return base64url(new Uint8Array(digest));
}

/** A fresh sign-in: new state, nonce and verifier, each used once. */
export function newPendingSignIn(returnTo: string, redirectUri: string, now = Date.now()): PendingSignIn {
  return {
    state: randomToken(),
    nonce: randomToken(),
    verifier: randomToken(),
    returnTo,
    redirectUri,
    startedAt: now,
  };
}

/** Where to send the browser to sign in. */
export async function authorizationUrl(
  provider: ProviderMetadata,
  clientId: string,
  pending: PendingSignIn,
): Promise<string> {
  const url = new URL(provider.authorization_endpoint);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", clientId);
  url.searchParams.set("redirect_uri", pending.redirectUri);
  url.searchParams.set("scope", SCOPES);
  url.searchParams.set("state", pending.state);
  url.searchParams.set("nonce", pending.nonce);
  url.searchParams.set("code_challenge", await challengeOf(pending.verifier));
  url.searchParams.set("code_challenge_method", "S256");
  return url.toString();
}

/**
 * The code the provider sent back, once the callback is proved to answer the
 * sign-in this browser started: the same `state`, not too old.
 */
export function codeFromCallback(
  callback: URL,
  pending: PendingSignIn | null,
  now = Date.now(),
): { code: string; pending: PendingSignIn } {
  const state = callback.searchParams.get("state");
  if (!pending || !state || state !== pending.state) {
    throw new SignInError(
      "state_mismatch",
      "This sign-in wasn't started from this browser tab, or was already used. Sign in again.",
    );
  }
  if (now - pending.startedAt > PENDING_MAX_AGE_MS) {
    throw new SignInError("sign_in_expired", "Sign-in took too long to finish. Sign in again.");
  }
  const error = callback.searchParams.get("error");
  if (error === "access_denied") {
    throw new SignInError(
      "consent_denied",
      "Sign-in was cancelled at the identity provider. Sign in again to continue.",
    );
  }
  const code = callback.searchParams.get("code");
  if (error || !code) {
    // The provider's own description isn't shown: it's the provider's text, not ours.
    throw new SignInError(
      "provider_refused",
      "The identity provider didn't sign you in. Check that this Console's address is a registered redirect URI for its client, then sign in again.",
    );
  }
  return { code, pending };
}

/** An ID token's claims, read without verifying its signature: the API verifies it. */
export function claimsOf(idToken: string): Record<string, unknown> | null {
  const payload = idToken.split(".")[1];
  if (!payload) return null;
  try {
    const padded = payload.replace(/-/g, "+").replace(/_/g, "/");
    const bytes = Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
    const claims: unknown = JSON.parse(new TextDecoder().decode(bytes));
    return claims && typeof claims === "object" && !Array.isArray(claims) ? (claims as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/**
 * Checks that an ID token belongs to this sign-in, and returns when it
 * expires and whose it is. `nonce` is checked when the token came from a code
 * exchange; a refreshed one has none to check against, but must name the same
 * `subject` as the token it replaces. A token for several audiences must name
 * the Console as its authorized party (`azp`, OIDC Core 3.1.3.7).
 */
export function checkIdToken(
  idToken: string,
  expected: { issuer: string; clientId: string; nonce: string | null; subject?: string },
  now = Date.now(),
): { expiresAt: number; subject: string } {
  const claims = claimsOf(idToken);
  const audience = claims?.aud;
  const audiences = Array.isArray(audience) ? audience : [audience];
  const nonceOk = expected.nonce === null || claims?.nonce === expected.nonce;
  const partyOk = audiences.length === 1 || claims?.azp === expected.clientId;
  const subject = claims?.sub;
  const exp = claims?.exp;
  if (
    !claims ||
    claims.iss !== expected.issuer ||
    !audiences.includes(expected.clientId) ||
    !partyOk ||
    !nonceOk ||
    typeof subject !== "string" ||
    subject === "" ||
    (expected.subject !== undefined && subject !== expected.subject) ||
    typeof exp !== "number"
  ) {
    throw new SignInError(
      "invalid_id_token",
      "The identity provider answered with a token that isn't for this sign-in. Sign in again; if it keeps happening, check that ZUPFLOW_OIDC_AUDIENCE is this Console's client ID.",
    );
  }
  if (exp * 1000 + CLOCK_SKEW_MS <= now) {
    throw new SignInError(
      "id_token_expired",
      "The identity provider answered with a sign-in that has already expired. Check that this device's clock is right, then sign in again.",
    );
  }
  return { expiresAt: exp * 1000, subject };
}

async function tokenRequest(
  provider: ProviderMetadata,
  form: Record<string, string>,
  fetchFn: typeof fetch,
): Promise<{ idToken: string; refreshToken: string | null }> {
  const failed = () =>
    new SignInError(
      "token_exchange_failed",
      "The identity provider didn't finish signing you in. Check that its client for this Console needs no secret and allows this origin, then sign in again.",
    );
  let body: Record<string, unknown>;
  try {
    const response = await fetchFn(provider.token_endpoint, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
      body: new URLSearchParams(form).toString(),
      redirect: "error",
    });
    if (!response.ok) throw failed();
    body = (await response.json()) as Record<string, unknown>;
  } catch {
    throw failed();
  }
  if (typeof body.id_token !== "string") throw failed();
  return {
    idToken: body.id_token,
    refreshToken: typeof body.refresh_token === "string" ? body.refresh_token : null,
  };
}

/** Exchanges the callback's code for tokens, with the sign-in's verifier, and checks the ID token. */
export async function exchangeCode(
  provider: ProviderMetadata,
  clientId: string,
  pending: PendingSignIn,
  code: string,
  fetchFn: typeof fetch = fetch,
  now = Date.now(),
): Promise<TokenSet> {
  const tokens = await tokenRequest(
    provider,
    {
      grant_type: "authorization_code",
      code,
      redirect_uri: pending.redirectUri,
      client_id: clientId,
      code_verifier: pending.verifier,
    },
    fetchFn,
  );
  const checked = checkIdToken(tokens.idToken, { issuer: provider.issuer, clientId, nonce: pending.nonce }, now);
  return { ...tokens, ...checked };
}

/** A fresh ID token from a refresh token, when the provider issued one, for the same person. */
export async function refreshTokens(
  provider: ProviderMetadata,
  clientId: string,
  previous: { refreshToken: string; subject: string },
  fetchFn: typeof fetch = fetch,
  now = Date.now(),
): Promise<TokenSet> {
  const { refreshToken, subject } = previous;
  const tokens = await tokenRequest(
    provider,
    { grant_type: "refresh_token", refresh_token: refreshToken, client_id: clientId },
    fetchFn,
  );
  const checked = checkIdToken(tokens.idToken, { issuer: provider.issuer, clientId, nonce: null, subject }, now);
  return { idToken: tokens.idToken, ...checked, refreshToken: tokens.refreshToken ?? refreshToken };
}

/**
 * Only a path in the Console: resolved the way the browser would, it must
 * stay on the Console's own origin. Anything else, including what a browser
 * reads as another host (`//x`, `/\\x`, `/\t/x`), goes to `/`.
 */
export function safeReturnPath(value: string | null | undefined, origin = window.location.origin): string {
  if (!value?.startsWith("/")) return "/";
  try {
    const url = new URL(value, origin);
    return url.origin === origin ? `${url.pathname}${url.search}${url.hash}` : "/";
  } catch {
    return "/";
  }
}

/** What to tell the person when sign-in fails. */
export function signInFailureMessage(error: unknown): string {
  return error instanceof SignInError ? error.message : "Sign-in didn't finish. Sign in again.";
}
