/**
 * What the suite needs from a server under test: where it is, a Console
 * session for each persona, and a way back to the reference fixture. Nothing
 * else: the suite reaches the server only over HTTP, so the mock and the
 * backend are held to the same cases.
 */

/**
 * The people the suite signs in as:
 *
 * - `owner`: the only active owner of the `northwind` organization, and a
 *   member of no organization the outsider belongs to.
 * - `developer`: an active developer of `northwind`.
 * - `viewer`: an active viewer of `northwind`: read-only.
 * - `outsider`: an active owner of `larkspur`, and not a member of
 *   `northwind`.
 */
export type Persona = "owner" | "developer" | "viewer" | "outsider";

export interface Target {
  /** Where the API is mounted; route paths are appended to it. */
  baseUrl: string;
  /** A Console session token for the persona. Called per request, after `reset`. */
  token(persona: Persona): Promise<string>;
  /** Restore the reference fixture. Called before every test. */
  reset(): Promise<void>;
  /** The fetch to use; defaults to the global one. */
  fetch?: typeof fetch;
  /**
   * The operations the server serves, by `operationId`; all of them when
   * omitted. A case that needs another is skipped and names it, and the suite
   * never sends that request.
   */
  operations?: readonly string[];
  /**
   * Whether the server accepts the API keys it issues as credentials; it
   * does when omitted. On one that doesn't (the mock has no API keys), a case
   * that needs a key is skipped.
   */
  apiKeys?: boolean;
}

/**
 * The reference fixture every target loads on `reset`. The suite finds each
 * object by its slug, never by a fixed ID.
 *
 * Organization `northwind`, with:
 * - Workspaces `acme` and `beta-labs` (Production), and `acme-sandbox` and
 *   `qa-playground` (Sandbox).
 * - In `acme` and `beta-labs` each: an agent with a grant and a finished run, an
 *   installation, a Connect session and a workspace app of its own (`acme`'s
 *   is `acme-publishing`; `beta-labs` has none at reset, and the suite makes
 *   one). In `acme-sandbox`: an active agent and an installation. Somewhere
 *   in the organization: a run waiting for approval.
 * - At least one API key, webhook endpoint and catalog app.
 * - A webhook endpoint in each environment with frozen delivery history:
 *   pending (two attempts), succeeded (two), failed (eight), skipped (none),
 *   and a succeeded `webhook.test` (one, without a workspace), all with complete
 *   recorded attempt history and durations. Run events belong alternately to
 *   `acme` and `beta-labs` in Production, `acme-sandbox` and `qa-playground` in
 *   Sandbox. Pending rows have a next retry; terminal rows do not. No worker
 *   advances these fixture deliveries while the suite runs.
 * - Catalog apps `google-workspace` (OAuth, ready in Sandbox), `linear`
 *   (ready in Sandbox, incomplete in Production) and `notion` (configured in
 *   neither environment).
 * - A workspace app `echo-server` owned by `qa-playground`, with token auth
 *   through one input named `token`. Its provider accepts any token and
 *   rejects one containing `invalid`. Its tools include `echo`, which needs
 *   no approval, and not `echo_write`; `qa-playground` has a connected
 *   installation of it.
 * - At least one model in `GET /v1/models`.
 * - The default tool policy (never saved: `manual`, new tools at `approve`)
 *   and levels for its catalog apps' tools: in `google-workspace`, confirmed
 *   `allow` for `search_files`, `read_document` and `create_document`,
 *   `approve` for `send_email` and `deny` for `delete_file`, and
 *   `list_events` only suggested (`allow`, unconfirmed); in `linear`,
 *   confirmed `allow` for `search_issues`, `create_issue` and, where listed,
 *   `add_comment`. `acme`'s grants hold inside them (a grant elsewhere may
 *   not: tightening a level rewrites no grant), and `acme` has an installation
 *   of `google-workspace` that lists all six of its tools.
 * - Runs in Sandbox as well as Production, at least one of them in
 *   `acme-sandbox`. At reset no run is queued or running; each is finished
 *   or waiting for approval, so nothing changes on its own during a test.
 * - Connect settings with at least one return URL or allowed origin.
 * - Sandbox Connect settings that allow the return URL
 *   `http://localhost:3000/settings/integrations`.
 *
 * Organization `larkspur`, owned by the outsider, with workspaces `harbor`
 * (Production) and `harbor-sandbox` (Sandbox). In `harbor`: an agent with a
 * grant and a finished run, an installation, a Connect session and a workspace
 * app of its own; in the organization: a catalog app `linear`, ready in Sandbox,
 * an API key, a webhook endpoint and Sandbox Connect settings that do not
 * allow Northwind's return URL, and an invitation for the viewer, sent more
 * than `INVITATION_TTL_DAYS` ago: expired. Each organization's real IDs are
 * aimed at the other.
 * The Production webhook has the same five delivery scenarios, all non-test
 * events belonging to `harbor`.
 *
 * Every Sandbox workspace the suite uses as one (`acme-sandbox`,
 * `qa-playground`, `harbor-sandbox`) offers at least one catalog app.
 */
export const REFERENCE_FIXTURE = "northwind";
export const OTHER_ORGANIZATION = "larkspur";
