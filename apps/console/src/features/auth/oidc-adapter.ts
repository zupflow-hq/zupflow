/**
 * `oidc`: sign in at the deployment's OpenID Connect provider and send its
 * ID token as the bearer (decision 0028).
 *
 * What the browser keeps, and where:
 * - the sign-in in flight (state, nonce, PKCE verifier, where to return) in
 *   this tab's `sessionStorage`, from the redirect until the callback reads
 *   and removes it: it must survive the round trip to the provider;
 * - the ID token in this tab's `sessionStorage` until it expires, so a reload
 *   doesn't sign out; closing the tab forgets it;
 * - a refresh token, if the provider issues one, in memory only.
 *
 * Signing out forgets all three here; it doesn't end the provider's own
 * session, so signing in again may not ask for a password.
 */
import type { OidcSignIn } from "@zupflow/sdk";
import {
  authorizationUrl,
  codeFromCallback,
  discover,
  exchangeCode,
  newPendingSignIn,
  type PendingSignIn,
  type ProviderMetadata,
  refreshTokens,
  safeReturnPath,
  signInFailureMessage,
  type TokenSet,
} from "@/lib/oidc";
import type { AuthAdapter, SignInCompletion } from "./adapter";

const PENDING_KEY = "zupflow.oidc.pending";
const SESSION_KEY = "zupflow.oidc.session";
/** A token this close to expiring is refreshed first, or not sent. */
const EXPIRY_MARGIN_MS = 60_000;

interface StoredSession {
  idToken: string;
  expiresAt: number;
}

function readJson<T>(key: string): T | null {
  try {
    const raw = sessionStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

function writeJson(key: string, value: unknown | null) {
  try {
    if (value === null) sessionStorage.removeItem(key);
    else sessionStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage refused: the sign-in lasts for this page only.
  }
}

function storedSession(): StoredSession | null {
  const stored = readJson<StoredSession>(SESSION_KEY);
  return stored && typeof stored.idToken === "string" && typeof stored.expiresAt === "number" ? stored : null;
}

/** The provider's callback lands here; it must be a registered redirect URI. */
export function redirectUri(): string {
  return `${window.location.origin}/sign-in/callback`;
}

export function createOidcAdapter(settings: OidcSignIn, changed: () => void): AuthAdapter {
  const { issuer, client_id: clientId } = settings;
  let current = storedSession();
  // In memory only, with the subject its tokens must keep naming.
  let refresh: { refreshToken: string; subject: string } | null = null;
  let provider: Promise<ProviderMetadata> | null = null;
  let refreshing: Promise<string | null> | null = null;
  const completions = new Map<string, Promise<SignInCompletion>>();

  const metadata = () => {
    provider ??= discover(issuer).catch((error: unknown) => {
      provider = null;
      throw error;
    });
    return provider;
  };

  const keep = (tokens: TokenSet) => {
    current = { idToken: tokens.idToken, expiresAt: tokens.expiresAt };
    refresh = tokens.refreshToken ? { refreshToken: tokens.refreshToken, subject: tokens.subject } : null;
    writeJson(SESSION_KEY, current);
  };

  const fresh = (session: StoredSession | null) =>
    session !== null && session.expiresAt - EXPIRY_MARGIN_MS > Date.now();

  async function renew(): Promise<string | null> {
    const previous = refresh;
    if (!previous) return null;
    try {
      keep(await refreshTokens(await metadata(), clientId, previous));
      return current?.idToken ?? null;
    } catch {
      refresh = null;
      return null;
    }
  }

  return {
    controls: {
      mode: "oidc",
      issuer,
      async signIn(returnTo) {
        const pending = newPendingSignIn(safeReturnPath(returnTo), redirectUri());
        const url = await authorizationUrl(await metadata(), clientId, pending);
        writeJson(PENDING_KEY, pending);
        window.location.assign(url);
      },
      completeSignIn(callback) {
        // Once per callback: React may run the page's effect twice, and a
        // `state` is spent on first use.
        const key = callback.search;
        let completion = completions.get(key);
        if (!completion) {
          const stored = readJson<PendingSignIn>(PENDING_KEY);
          writeJson(PENDING_KEY, null);
          // Where the sign-in started, kept apart from its spent state.
          const returnTo = safeReturnPath(stored?.returnTo);
          completion = (async (): Promise<SignInCompletion> => {
            try {
              const { code, pending } = codeFromCallback(callback, stored);
              keep(await exchangeCode(await metadata(), clientId, pending, code));
              changed();
              return { ok: true, returnTo };
            } catch (error) {
              return { ok: false, returnTo, message: signInFailureMessage(error) };
            }
          })();
          completions.set(key, completion);
        }
        return completion;
      },
    },
    isSignedIn: () => fresh(current) || refresh !== null,
    async token() {
      if (fresh(current)) return current?.idToken ?? null;
      refreshing ??= renew().finally(() => {
        refreshing = null;
      });
      // Without a fresh token the request goes without one: the API answers
      // 401, and the Console sends the person back to sign in.
      return refreshing;
    },
    signOut() {
      current = null;
      refresh = null;
      writeJson(SESSION_KEY, null);
      writeJson(PENDING_KEY, null);
      changed();
    },
  };
}
