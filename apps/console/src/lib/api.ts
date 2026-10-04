import { createClient, createHttpTransport, type OrganizationId } from "@zupflow/sdk";
import { config } from "./env";

let tokenSource: () => Promise<string | null> = async () => null;

/** Called by the auth provider so every request carries a fresh credential. */
export function setTokenSource(source: () => Promise<string | null>) {
  tokenSource = source;
}

let organizationSource: () => OrganizationId | null = () => null;

/**
 * How the Console knows which organization a request acts in. Resolved when
 * each request is sent, from the page being shown, so a preloaded link to
 * another organization can't change it underneath the current page.
 */
export function setOrganizationSource(source: () => OrganizationId | null) {
  organizationSource = source;
}

export const transport = createHttpTransport({
  baseUrl: config.apiUrl,
  getToken: () => tokenSource(),
  getOrganization: () => organizationSource(),
});

export const api = createClient(transport);
