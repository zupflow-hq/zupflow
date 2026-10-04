import { createClient, createHttpTransport } from "@zupflow/sdk";
import { config } from "./env";

let tokenSource: () => Promise<string | null> = async () => null;

/** Called by the auth provider so every request carries a fresh credential. */
export function setTokenSource(source: () => Promise<string | null>) {
  tokenSource = source;
}

export const transport = createHttpTransport({ baseUrl: config.apiUrl, getToken: () => tokenSource() });

export const api = createClient(transport);
