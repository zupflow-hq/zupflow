/**
 * Checks HTTP responses against the contract: the route's success status and
 * response schema with no field the contract doesn't name, a strict
 * `{ data, meta, error }` envelope with pagination where the route promises
 * it, and the error envelope on failures.
 */
import { isDeepStrictEqual } from "node:util";
import { ApiErrorBody, Meta, matchRoute, Pagination, responseDataSchema } from "@zupflow/contract";
import { z } from "zod";

export interface ResponseCheckerOptions {
  /** Where the API is mounted, e.g. `http://localhost:8080` or `http://zupflow.test/api`. */
  baseUrl: string;
  /** Requests to leave unchecked, such as a mock's own control routes. */
  ignore?: (method: string, path: string) => boolean;
  /** The fetch to check. Resolved per request, so later instrumentation still applies. */
  fetch?: typeof fetch;
}

export interface ResponseChecker {
  /** A fetch that checks every response it returns. */
  readonly fetch: typeof fetch;
  /** Check one response; what doesn't match is recorded, not thrown. */
  check(request: Request, response: Response): Promise<void>;
  /** The violations recorded since the last call. */
  takeViolations(): string[];
  /** The operations whose success response has passed the check. */
  readonly succeeded: Set<string>;
}

export function createResponseChecker(options: ResponseCheckerOptions): ResponseChecker {
  const base = new URL(options.baseUrl).pathname.replace(/\/$/, "");
  const violations: string[] = [];
  const succeeded = new Set<string>();

  async function check(request: Request, response: Response) {
    const url = new URL(request.url);
    if (!url.pathname.startsWith(`${base}/`)) return;
    const path = url.pathname.slice(base.length);
    const where = `${request.method} ${path}`;
    if (options.ignore?.(request.method, path)) return;
    const before = violations.length;
    const route = matchRoute(request.method, path);
    if (!route) {
      violations.push(`${where}: no route in the contract`);
      return;
    }
    if (response.status === 204) {
      if (route.response.status !== 204) {
        violations.push(`${where}: answered 204, contract says ${route.response.status}`);
      } else if ((await response.text()) !== "") {
        violations.push(`${where}: answered 204 with a body`);
      } else {
        succeeded.add(route.operationId);
      }
      return;
    }
    const body = (await response.json()) as { data: unknown; meta: unknown; error: unknown };
    if (!response.ok) {
      const error = ApiErrorBody.safeParse(body.error);
      if (!error.success || body.data !== null || body.meta !== null) {
        violations.push(`${where}: error body isn't the error envelope`);
      } else if (!isDeepStrictEqual(error.data, body.error)) {
        violations.push(`${where}: error fields beyond the contract`);
      }
      return;
    }
    if (response.status !== route.response.status) {
      violations.push(`${where}: answered ${response.status}, contract says ${route.response.status}`);
    }
    // The envelope itself: exactly data, meta and error, with meta as the contract shapes it.
    const meta = route.response.paginated ? Meta.extend({ pagination: Pagination }) : Meta.nullable();
    const envelope = z.strictObject({ data: z.unknown(), meta, error: z.null() }).safeParse(body);
    if (!envelope.success || !isDeepStrictEqual(envelope.data, body)) {
      violations.push(`${where}: envelope isn't { data, meta, error } as the contract shapes it`);
    }
    const schema = responseDataSchema(route);
    if (schema) {
      const parsed = schema.safeParse(body.data);
      if (!parsed.success) {
        violations.push(`${where}: ${z.prettifyError(parsed.error)}`);
        return;
      }
      // Parsing drops fields the contract doesn't name; anything dropped is a leak.
      if (!isDeepStrictEqual(parsed.data, body.data)) violations.push(`${where}: fields beyond the contract`);
    }
    if (violations.length === before) succeeded.add(route.operationId);
  }

  const inner: typeof fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
  return {
    check,
    succeeded,
    takeViolations: () => violations.splice(0),
    fetch: async (input, init) => {
      const request = new Request(input, init);
      const response = await inner(request.clone());
      await check(request, response.clone());
      return response;
    },
  };
}
