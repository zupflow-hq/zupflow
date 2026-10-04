/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_ZUPFLOW_API_URL?: string;
  readonly VITE_ZUPFLOW_MOCK?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
