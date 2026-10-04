import type { OrganizationId } from "./ids";
import type { ApiErrorBody, Envelope, Meta } from "./types";

/**
 * Names the organization a Console session acts in. Workspace routes
 * (`/v1/workspaces/{ws}`) don't carry it, and a member can belong to several
 * organizations, so the session says which one; the API checks it against the
 * membership and the workspace. API keys don't need it: a key belongs to one
 * organization.
 */
export const ORGANIZATION_HEADER = "Zupflow-Organization";

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export type QueryValue = string | number | boolean | null | undefined;

export interface RequestOptions {
  method: HttpMethod;
  path: string;
  query?: Record<string, QueryValue>;
  body?: unknown;
  signal?: AbortSignal;
}

export interface TransportResponse<T> {
  data: T;
  meta: Meta | null;
}

/**
 * The only way the SDK reaches the network. Feature code never builds its own
 * authenticated `fetch`.
 */
export interface Transport {
  request<T>(options: RequestOptions): Promise<TransportResponse<T>>;
}

export class ZupflowApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly fields: Record<string, string> | null;

  constructor(status: number, body: ApiErrorBody) {
    super(body.message);
    this.name = "ZupflowApiError";
    this.status = status;
    this.code = body.code;
    this.fields = body.fields;
  }
}

export class ZupflowNetworkError extends Error {
  constructor(cause: unknown) {
    super("The Zupflow API could not be reached.", { cause });
    this.name = "ZupflowNetworkError";
  }
}

export interface HttpTransportOptions {
  baseUrl: string;
  /**
   * Resolves a fresh bearer credential for each request. Returning `null`
   * sends the request unauthenticated; the server answers 401.
   */
  getToken: () => Promise<string | null>;
  /** For Console sessions: the organization each request acts in. */
  getOrganization?: () => OrganizationId | null;
  fetch?: typeof fetch;
}

function buildUrl(baseUrl: string, path: string, query?: Record<string, QueryValue>): string {
  const url = new URL(path.replace(/^\//, ""), baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  if (query) {
    for (const [key, value] of Object.entries(query)) {
      if (value === undefined || value === null || value === "") continue;
      url.searchParams.set(key, String(value));
    }
  }
  return url.toString();
}

function isEnvelope(value: unknown): value is Envelope<unknown> {
  return typeof value === "object" && value !== null && "data" in value && "meta" in value && "error" in value;
}

export function createHttpTransport(options: HttpTransportOptions): Transport {
  // Resolve fetch per request so later instrumentation or polyfills apply.
  const doFetch: typeof fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));

  return {
    async request<T>({ method, path, query, body, signal }: RequestOptions) {
      const headers = new Headers({ Accept: "application/json" });
      const token = await options.getToken();
      if (token) headers.set("Authorization", `Bearer ${token}`);
      const organization = options.getOrganization?.();
      if (organization) headers.set(ORGANIZATION_HEADER, organization);
      if (body !== undefined) headers.set("Content-Type", "application/json");

      let response: Response;
      try {
        response = await doFetch(buildUrl(options.baseUrl, path, query), {
          method,
          headers,
          body: body === undefined ? undefined : JSON.stringify(body),
          signal,
        });
      } catch (error) {
        if (error instanceof DOMException && error.name === "AbortError") throw error;
        throw new ZupflowNetworkError(error);
      }

      if (response.status === 204) {
        return { data: undefined as T, meta: null };
      }

      let payload: unknown;
      try {
        payload = await response.json();
      } catch {
        payload = null;
      }

      if (!isEnvelope(payload)) {
        throw new ZupflowApiError(response.status, {
          code: response.ok ? "invalid_response" : "http_error",
          message: response.ok
            ? "The API returned a response the Console could not read."
            : `The API answered ${response.status}.`,
          fields: null,
        });
      }

      if (!response.ok || payload.error) {
        throw new ZupflowApiError(
          response.status,
          payload.error ?? {
            code: "http_error",
            message: `The API answered ${response.status}.`,
            fields: null,
          },
        );
      }

      return { data: payload.data as T, meta: payload.meta };
    },
  };
}
