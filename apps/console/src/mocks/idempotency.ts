/**
 * `Idempotency-Key` for every `POST` (decision 0016): one wrapper inside
 * `route`, following the rules the backend's middleware follows. Handlers know
 * nothing about it.
 *
 * - The caller is resolved first, exactly as the handler will resolve it, so
 *   `401` and `404` are never replayed past their checks.
 * - A key belongs to that caller: organization, workspace (on workspace
 *   routes), environment and credential.
 * - The same key and request replays the saved status and body; a different
 *   request answers `422`, one still in flight `409`.
 * - `5xx` saves nothing. A route that shows a secret once saves that its key
 *   completed, never the body. One whose answer holds what people wrote saves
 *   only the ID of the memory it saved, and a retry answers that memory as it
 *   is now, or `409 idempotency_key_completed` once it's forgotten, or when
 *   its kind can't list memories.
 */
import {
  IDEMPOTENCY_HEADER,
  IDEMPOTENCY_KEY_PATTERN,
  IDEMPOTENCY_KEY_TTL_MS,
  IDEMPOTENCY_LOCK_MS,
  matchRoute,
  type Route,
} from "@zupflow/contract";
import { HttpResponse } from "msw";
import { keyedDigest } from "./credentials";
import { db, persist } from "./db";
import {
  ApiFailure,
  authenticate,
  findSessionByReferenceCrossTenant,
  orgAccess,
  type SavedAgain,
  workspaceAccess,
} from "./http";
import type { IdempotencyRow } from "./model";

type Params = Record<string, string | readonly string[] | undefined>;
type Scope = Pick<IdempotencyRow, "organization_id" | "workspace_id" | "caller">;

/**
 * Who sent the request, resolved the way the handler resolves it: 401, then
 * the organization, then the workspace. Fails closed: a route whose scope it
 * doesn't recognise is an error, never a key without an organization.
 */
async function scopeOf(route: Route, request: Request, params: Params): Promise<Scope> {
  const param = (name: string) => {
    const value = params[name];
    return typeof value === "string" ? value : undefined;
  };
  const workspace = param("ws");
  if (route.path.startsWith("/v1/workspaces/{ws}") && workspace !== undefined) {
    const access = workspaceAccess(request, workspace);
    return {
      organization_id: access.org.id,
      workspace_id: access.workspace.id,
      caller:
        access.kind === "api_key"
          ? `api_key:${access.key.id}|${access.key.environment}`
          : `member:${access.user.id}|${access.workspace.environment}`,
    };
  }
  const organization = param("org");
  if (route.path.startsWith("/v1/organizations/{org}") && organization !== undefined) {
    const access = orgAccess(request, organization);
    return {
      organization_id: access.org.id,
      workspace_id: null,
      caller:
        access.kind === "api_key" ? `api_key:${access.key.id}|${access.key.environment}` : `member:${access.user.id}|-`,
    };
  }
  const reference = param("reference");
  if (route.path.startsWith("/v1/connect/sessions/{reference}") && reference !== undefined) {
    // Hosted Connect is authorized by the reference alone; the session is the credential.
    const session = await findSessionByReferenceCrossTenant(reference);
    return {
      organization_id: session.organization_id,
      workspace_id: session.workspace_id,
      caller: `connect:${session.id}|${session.environment}`,
    };
  }
  if (
    route.operationId === "createOrganization" ||
    route.operationId === "acceptInvitation" ||
    route.operationId === "declineInvitation"
  ) {
    // No organization yet, or the person's own invitations: these keys belong to the user alone.
    return { organization_id: null, workspace_id: null, caller: `member:${authenticate(request).id}|-` };
  }
  throw new Error(`No idempotency scope for ${route.operationId}`);
}

/** What a route whose answer is never kept keeps of a success: the ID of the memory it saved, `data.id`. */
async function savedId(response: Response): Promise<string | null> {
  const answer: unknown = await response
    .clone()
    .json()
    .catch(() => null);
  const id = (answer as { data?: { id?: unknown } } | null)?.data?.id;
  return typeof id === "string" ? id : null;
}

/** What a retry of a save is told when it can't be answered with the memory it saved. */
const UNSHOWN = {
  forgotten:
    "This request already completed, and the memory it saved has since been forgotten. Start again with a new key to save it again.",
  unlisted:
    "This request already completed, and the memory it saved is of a kind that can't list memories from the API or the Console, so it isn't shown again.",
  unkept:
    "This request already completed, and its response held a memory's text, which is never kept. Find the memory in the agent's list, or start again with a new key.",
} as const;

function replay(row: IdempotencyRow): Response {
  const status = row.status ?? 200;
  return new HttpResponse(status === 204 ? null : row.body, {
    status,
    headers: status === 204 ? {} : { "Content-Type": "application/json" },
  });
}

const sameKey = (scope: Scope, key: string) => (r: IdempotencyRow) =>
  r.organization_id === scope.organization_id &&
  r.workspace_id === scope.workspace_id &&
  r.caller === scope.caller &&
  r.key === key;

/**
 * Run `handle` at most once per key. `handle` returns the final response,
 * errors included; this throws {@link ApiFailure} for its own answers. A route
 * whose answer is never kept answers a retry with `answerAgain`, from the ID
 * of the memory it saved.
 */
export async function idempotently(
  request: Request,
  params: Params,
  handle: () => Promise<Response>,
  answerAgain?: (id: string) => SavedAgain,
) {
  const url = new URL(request.url);
  const path = url.pathname.slice(Math.max(0, url.pathname.indexOf("/v1/")));
  const route = request.method === "POST" ? matchRoute("POST", path) : undefined;
  const key = request.headers.get(IDEMPOTENCY_HEADER);
  // Mock-only routes have no contract entry, a provider's callback is keyed by its own event ID, and a
  // request without a key just runs.
  if (!route || key === null || route.callers.includes("provider")) return handle();

  const scope = await scopeOf(route, request, params);
  if (!IDEMPOTENCY_KEY_PATTERN.test(key)) {
    throw new ApiFailure(
      400,
      "invalid_idempotency_key",
      `${IDEMPOTENCY_HEADER} must be 1–255 printable ASCII characters.`,
      { [IDEMPOTENCY_HEADER]: "1–255 printable ASCII characters." },
    );
  }
  // Only the route's template is stored in the clear: a hosted Connect path carries its reference, so the
  // actual path goes into the keyed digest with the query and the body.
  const fingerprint = `POST ${route.path} ${await keyedDigest("idempotency", `${path}${url.search}\n${await request.clone().text()}`)}`;

  // From here to the insert there is no await, so two requests can't both start.
  const now = Date.now();
  const live = (r: IdempotencyRow) => Date.parse(r.created_at) + IDEMPOTENCY_KEY_TTL_MS > now;
  const d = db();
  // Expired keys count as unused; this caller's organization sheds them as it goes.
  d.idempotency_keys = d.idempotency_keys.filter((r) => r.organization_id !== scope.organization_id || live(r));
  const row = d.idempotency_keys.find(sameKey(scope, key));
  if (row) {
    if (row.fingerprint !== fingerprint) {
      throw new ApiFailure(
        422,
        "idempotency_key_reused",
        "This idempotency key was already used for a different request. Use a new key for a new request.",
      );
    }
    if (row.state === "completed") {
      if (route.answerNeverKept && row.status !== null && row.status >= 200 && row.status < 300) {
        // Only the ID of the memory it saved was kept: that memory, as a read shows it now.
        const again = row.body === null ? undefined : answerAgain?.(row.body);
        if (typeof again === "object") return again;
        throw new ApiFailure(409, "idempotency_key_completed", UNSHOWN[again ?? "unkept"]);
      }
      if (row.body === null && row.status !== 204) {
        throw new ApiFailure(
          409,
          "idempotency_key_completed",
          "This request already completed, and its response showed a secret that is never shown twice. Find the resource in its list, or start again with a new key.",
        );
      }
      return replay(row);
    }
    if (Date.parse(row.locked_until) > now) {
      throw new ApiFailure(
        409,
        "idempotency_request_in_progress",
        "A request with this idempotency key is still in progress. Retry in a moment.",
      );
    }
    // A request that never finished: its lock lapsed, so this one takes the key over.
    row.locked_until = new Date(now + IDEMPOTENCY_LOCK_MS).toISOString();
  } else {
    d.idempotency_keys.push({
      ...scope,
      key,
      fingerprint,
      state: "running",
      status: null,
      body: null,
      created_at: new Date(now).toISOString(),
      locked_until: new Date(now + IDEMPOTENCY_LOCK_MS).toISOString(),
    });
  }
  persist();

  const response = await handle();
  // An error carries no secret and no one's words; a success is kept unless its route says never to, and one
  // whose answer holds what people wrote keeps only the ID of the memory it saved.
  const body = !response.ok
    ? await response.clone().text()
    : route.showsSecretOnce
      ? null
      : route.answerNeverKept
        ? await savedId(response)
        : await response.clone().text();
  // Another tab may have written while the handler ran, so find the row again in the current state.
  const store = db();
  if (response.status >= 500) {
    // Nothing is saved for a server error, so a retry runs again.
    store.idempotency_keys = store.idempotency_keys.filter((r) => !sameKey(scope, key)(r));
  } else {
    const saved = store.idempotency_keys.find(sameKey(scope, key));
    if (saved) {
      saved.state = "completed";
      saved.status = response.status;
      saved.body = body;
    }
  }
  persist();
  return response;
}
