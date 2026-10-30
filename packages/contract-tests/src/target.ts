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

/** What a channel provider holds for one generated app: where it delivers events, and the secret it signs them with. */
export interface ChannelAppSecrets {
  inbound_ref: string;
  signing_secret: string;
}

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
   * What the channel provider knows about one of the server's generated apps
   * (`chapp_…`): read out of band, as the provider holds it, never through
   * the API, which shows neither. Cases that deliver provider events are
   * skipped, naming `receiveChannelEvent`, without it.
   */
  channelAppSecrets?: (app: string) => Promise<ChannelAppSecrets>;
}

/**
 * The reference fixture every target loads on `reset`. The suite finds each
 * object by its slug (an app by its key), never by a fixed ID.
 *
 * Organization `northwind`, with:
 * - Workspaces `acme` and `beta-labs` (Production), and `acme-sandbox` and
 *   `qa-playground` (Sandbox).
 * - In `acme` and `beta-labs` each: an agent with a grant and a finished run, an
 *   installation, a Connect session and a workspace app of its own (`acme`'s
 *   is `acme_publishing`; `beta-labs` has none at reset, and the suite makes
 *   one). In `acme-sandbox`: an active agent and an installation. Somewhere
 *   in the organization: a run waiting for approval in each environment; the
 *   Sandbox one belongs to `qa-playground` and waits on `echo_write`.
 * - At least one API key, webhook endpoint and catalog app. Fixture keys are
 *   metadata only: tests create fresh credentials through the Console API.
 * - A webhook endpoint in each environment with frozen delivery history:
 *   pending (two attempts), succeeded (two), failed (eight), skipped (none),
 *   and a succeeded `webhook.test` (one, without a workspace), all with complete
 *   recorded attempt history and durations. Run events belong alternately to
 *   `acme` and `beta-labs` in Production, `acme-sandbox` and `qa-playground` in
 *   Sandbox. Pending rows have a next retry; terminal rows do not. No worker
 *   advances these fixture deliveries while the suite runs.
 * - Catalog apps `google_workspace` (OAuth, ready in Sandbox), `taskboard`
 *   (configured OAuth, ready in Sandbox, incomplete in Production) and `notion`
 *   (configured in neither environment). The static Taskboard URLs use
 *   `https://taskboard.example`; live MCP OAuth journeys create `taskboard_live`.
 * - `acme-sandbox`'s Taskboard registry has `list_tasks` current,
 *   `create_task` with changed text, and `delete_task` with changed hints.
 *   Synthetic historical `export_tasks` is gone; it is not a tool Taskboard serves.
 * - A workspace app `echo_server` owned by `qa-playground`, with token auth
 *   through one input named `token`. Its provider accepts any token and
 *   rejects one containing `invalid`. Its tools include `echo`, which needs
 *   no approval, and `echo_write`, which does; `qa-playground` has a connected
 *   installation of it.
 * - Model providers in both environments, each with a default model. In
 *   Sandbox: an active OpenRouter connection and an active Anthropic one. In
 *   Production: active Anthropic and OpenAI connections, and an OpenRouter
 *   connection whose key the provider refused (`invalid_key`). Providers
 *   accept any key of the right shape (20 to 500 characters, no spaces) and
 *   refuse one containing `invalid`. OpenAI and OpenRouter each list, among
 *   their first 100 models, at least one that can't call tools.
 *   At least one agent in each environment overrides the default.
 * - The default tool policy (never saved: `manual`, new tools at `approve`)
 *   and levels for its catalog apps' tools: in `google_workspace`, confirmed
 *   `allow` for `search_files`, `read_document` and `create_document`,
 *   `approve` for `send_email` and `deny` for `delete_file`, and
 *   `list_events` only suggested (`allow`, unconfirmed); in `taskboard`,
 *   confirmed `allow` for `list_tasks`, `create_task` and, in Northwind,
 *   `delete_task`. No seeded grant includes `delete_task`; live journeys separately
 *   prove rejection of denied destructive calls. `acme`'s grants hold inside them
 *   (a grant elsewhere may not: tightening a level rewrites no grant), and `acme` has an installation
 *   of `google_workspace` that lists all six of its tools.
 * - Runs in Sandbox as well as Production, at least one of them in
 *   `acme-sandbox`. At reset no run is queued or running; each is finished
 *   or waiting for approval, so nothing changes on its own during a test.
 * - Connect settings with at least one return URL or allowed origin.
 * - Sandbox Connect settings that allow the return URL
 *   `http://localhost:3000/settings/integrations`.
 * - Channels in `acme`, `beta-labs` and `acme-sandbox`: each a linked Slack
 *   team (an `active` app manager), one agent's `installed` app with its
 *   `connected` connection, that agent's channel on it with the defaults
 *   except tools `read_thread`, `reply` and `send_message` (approval for
 *   `send_message`), a person, and two conversations: `support` (a channel,
 *   with a three-message thread, one message the agent's own) and a DM with
 *   one message. In `acme-sandbox`, a second active agent with no app.
 *   Channel tool levels and channel settings at their defaults (never saved).
 *   The channel provider accepts any configuration token and refresh token,
 *   and refuses one containing `invalid`.
 * - The `notes` memory kind served. Memory types, a default memory type and
 *   memories are optional, here and in `larkspur`: the suite makes the types,
 *   agents and memories it needs (a memory for each tenant's agent, saved with
 *   an API key of its workspace's environment), and passes with or without any
 *   at reset.
 *
 * Organization `larkspur`, owned by the outsider, with workspaces `harbor`
 * (Production) and `harbor-sandbox` (Sandbox). In `harbor`: an agent with a
 * grant and a finished run, an installation, a Connect session and a workspace
 * app of its own; in the organization: a catalog app `taskboard`, ready in Sandbox,
 * an API key, a webhook endpoint, Sandbox Connect settings that do not
 * allow Northwind's return URL, a model connection with a model in each
 * environment (Production's is its default; Sandbox has none), and an invitation for the viewer, sent more
 * than `INVITATION_TTL_DAYS` ago: expired. Each organization's real IDs are
 * aimed at the other.
 * The Production webhook has the same five delivery scenarios, all non-test
 * events belonging to `harbor`. `harbor` has channels as `acme` does.
 *
 * Every Sandbox workspace the suite uses as one (`acme-sandbox`,
 * `qa-playground`, `harbor-sandbox`) offers at least one catalog app.
 */
export const REFERENCE_FIXTURE = "northwind";
export const OTHER_ORGANIZATION = "larkspur";
