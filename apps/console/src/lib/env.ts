/**
 * Runtime configuration. With no `VITE_ZUPFLOW_API_URL` the Console runs
 * against the in-browser mock API, which serves the same contracts.
 */
const configured = import.meta.env.VITE_ZUPFLOW_API_URL as string | undefined;

export const config = {
  apiUrl: configured?.trim() || `${window.location.origin}/api`,
  mockApi: !configured?.trim() || import.meta.env.VITE_ZUPFLOW_MOCK === "1",
} as const;
