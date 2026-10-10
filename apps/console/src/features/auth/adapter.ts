/**
 * An auth adapter: how the Console signs in and what it sends as its
 * credential (decision 0020). The open-source Console has three: `local` and
 * `oidc`, picked from what the API says (`GET /v1/sign-in-settings`), and the
 * mock's simulated sign-in, which exists only in development builds.
 */

export interface MockControls {
  mode: "mock";
  /** Signs in as the seeded demo owner. */
  signInAsDemo(): void;
  /** A brand new identity with no organizations yet. */
  signUp(input: { name: string; email: string }): void;
  /** Reseeds the mock's data and signs in as the demo owner again. */
  resetDemoData(): void;
}

export interface LocalControls {
  mode: "local";
}

export interface OidcControls {
  mode: "oidc";
  /** The provider people sign in at, as the API names it. */
  issuer: string;
  /** Sends the browser to the provider; it comes back to `/sign-in/callback`. */
  signIn(returnTo: string): Promise<void>;
  /**
   * Finishes a sign-in from the callback URL. Either way it names the page the
   * sign-in started from, so a retry after a failure returns there too.
   */
  completeSignIn(callback: URL): Promise<SignInCompletion>;
}

export type SignInCompletion = { ok: true; returnTo: string } | { ok: false; returnTo: string; message: string };

export interface UnavailableControls {
  mode: "unavailable";
  /** Where the Console looked for the API. */
  apiUrl: string;
  /** What went wrong, and what to check. */
  reason: string;
}

/** What the sign-in screens need from the adapter in use. */
export type SessionControls = MockControls | LocalControls | OidcControls | UnavailableControls;

export interface AuthAdapter {
  readonly controls: SessionControls;
  isSignedIn(): boolean;
  /** The bearer for the next request, or `null` to send none. */
  token(): Promise<string | null>;
  signOut(): void;
}
