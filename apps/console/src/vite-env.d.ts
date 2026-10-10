/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_ZUPFLOW_API_URL?: string;
  readonly VITE_ZUPFLOW_MOCK?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

/**
 * Whether this build contains the mock API: true in development and tests,
 * and in a build made with `VITE_ZUPFLOW_MOCK=1`. Replaced at build time, so
 * code behind `if (__ZUPFLOW_MOCK__)` is dropped from a production build.
 */
declare const __ZUPFLOW_MOCK__: boolean;
