/**
 * Runtime configuration. With no `VITE_ZUPFLOW_API_URL`, a development build
 * runs against the in-browser mock API, which serves the same contracts; a
 * production build calls the API at `/api` on its own origin, and contains
 * the mock only when built with `VITE_ZUPFLOW_MOCK=1`.
 */
const configured = import.meta.env.VITE_ZUPFLOW_API_URL as string | undefined;

export const config = {
  apiUrl: configured?.trim() || `${window.location.origin}/api`,
  mockApi: __ZUPFLOW_MOCK__ && (!configured?.trim() || import.meta.env.VITE_ZUPFLOW_MOCK === "1"),
} as const;
