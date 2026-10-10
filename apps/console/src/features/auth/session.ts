/**
 * The Console's session boundary: every page asks this module whether
 * someone is signed in, and every request gets its credential from it.
 *
 * `startSession` picks the auth adapter before the app renders. Against a
 * real API it asks the API how to sign in (`GET /v1/sign-in-settings`):
 * `local` signs nobody in (the server is the built-in owner), `oidc` signs in
 * at the provider and sends its ID token. The mock's simulated sign-in loads
 * only in development and test builds, or a build made with
 * `VITE_ZUPFLOW_MOCK=1`: `__ZUPFLOW_MOCK__` is a build-time constant, so a
 * production build drops the mock entirely (`scripts/check-build.mjs` holds
 * it to that).
 */
import { ZupflowApiError, ZupflowNetworkError } from "@zupflow/sdk";
import { api, setTokenSource } from "@/lib/api";
import { config } from "@/lib/env";
import type { AuthAdapter, SessionControls } from "./adapter";
import { createOidcAdapter } from "./oidc-adapter";

type Listener = () => void;
const listeners = new Set<Listener>();

function notify() {
  for (const listener of listeners) listener();
}

/** Before `startSession` finishes, nobody is signed in and nothing is sent. */
let adapter: AuthAdapter = unavailable(config.apiUrl, "The Console hasn't started yet.");

setTokenSource(() => adapter.token());

function unavailable(apiUrl: string, reason: string): AuthAdapter {
  return {
    controls: { mode: "unavailable", apiUrl, reason },
    isSignedIn: () => false,
    token: async () => null,
    signOut: () => {},
  };
}

const local: AuthAdapter = {
  controls: { mode: "local" },
  // The server signs every Console request in as its built-in owner.
  isSignedIn: () => true,
  token: async () => null,
  signOut: () => {},
};

function unreachable(error: unknown): string {
  const origin = window.location.origin;
  if (error instanceof ZupflowNetworkError) {
    return `The API didn't answer. Check that the server is running at this address, and that it allows this Console: ZUPFLOW_CONSOLE_ORIGIN=${origin}.`;
  }
  if (error instanceof ZupflowApiError && error.code === "origin_not_allowed") {
    return `The API refuses requests from this Console. Set ZUPFLOW_CONSOLE_ORIGIN=${origin} on the server and restart it.`;
  }
  if (error instanceof ZupflowApiError) {
    return `The API answered ${error.status} (${error.code}) when asked how to sign in. Check that this address is a Zupflow API.`;
  }
  return "The API's answer couldn't be read. Check that this address is a Zupflow API.";
}

/** Picks the adapter. Resolves once the Console knows how to sign in, or that it can't reach the API. */
export async function startSession(): Promise<void> {
  if (__ZUPFLOW_MOCK__ && config.mockApi) {
    const { createMockAdapter } = await import("./mock-adapter");
    adapter = await createMockAdapter(notify);
    return;
  }
  try {
    const settings = await api.signIn.settings();
    adapter = settings.mode === "local" ? local : createOidcAdapter(settings, notify);
  } catch (error) {
    adapter = unavailable(config.apiUrl, unreachable(error));
  }
}

export const session = {
  /** What the sign-in screens can do with the adapter in use. */
  controls: (): SessionControls => adapter.controls,
  isSignedIn: () => adapter.isSignedIn(),
  /** Whether signing out means anything: with `local`, nobody signs in. */
  canSignOut: () => adapter.controls.mode !== "local",
  subscribe(listener: Listener) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  signOut() {
    adapter.signOut();
  },
};
