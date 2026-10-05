/** Finds the reference fixture's objects through the API, by slug. */
import type { AppId, OrganizationId, Workspace, ZupflowClient } from "@zupflow/sdk";
import { OTHER_ORGANIZATION, REFERENCE_FIXTURE } from "./target";

export interface Fixture {
  organization: OrganizationId;
  /** The outsider's own organization. */
  otherOrganization: OrganizationId;
  workspaces: { acme: Workspace; acmeSandbox: Workspace; qaPlayground: Workspace };
  /** Throws, when read, whatever kept the apps from loading (a server that doesn't serve apps yet). */
  apps: { googleWorkspace: AppId; linear: AppId; notion: AppId; echoServer: AppId };
}

export function must<T>(value: T | null | undefined, what: string): T {
  if (value === null || value === undefined) throw new Error(`The reference fixture has no ${what}`);
  return value;
}

/**
 * `clientIn(org)` is the owner's Console session acting in `org`, or in none;
 * `outsider` is the outsider's session acting in none.
 */
export async function loadFixture(
  clientIn: (org: OrganizationId | null) => ZupflowClient,
  outsider: ZupflowClient,
): Promise<Fixture> {
  const viewer = await clientIn(null).me.get();
  const organization = must(
    viewer.organizations.find((o) => o.slug === REFERENCE_FIXTURE),
    `organization ${REFERENCE_FIXTURE}`,
  ).id;
  // The outsider is a real, signed-in user of another organization, and not of this one.
  const stranger = await outsider.me.get();
  if (stranger.organizations.some((o) => o.id === organization)) {
    throw new Error(`The outsider persona must not belong to ${REFERENCE_FIXTURE}`);
  }
  const otherOrganization = must(
    stranger.organizations.find((o) => o.slug === OTHER_ORGANIZATION),
    `organization ${OTHER_ORGANIZATION} for the outsider`,
  ).id;
  const client = clientIn(organization);
  const workspace = async (slug: string) => {
    const page = await client.workspaces.list(organization, { q: slug, limit: 100 });
    return must(
      page.items.find((w) => w.slug === slug),
      `workspace ${slug}`,
    );
  };
  const [acme, acmeSandbox, qaPlayground] = await Promise.all([
    workspace("acme"),
    workspace("acme-sandbox"),
    workspace("qa-playground"),
  ]);
  const loadApps = async (): Promise<Fixture["apps"]> => {
    const catalog = await client.organizationApps.list(organization);
    const own = await client.workspaceApps.list(qaPlayground.id);
    const app = (apps: typeof catalog, slug: string) =>
      must(
        apps.find((a) => a.slug === slug),
        `app ${slug}`,
      ).id;
    return {
      googleWorkspace: app(catalog, "google-workspace"),
      linear: app(catalog, "linear"),
      notion: app(catalog, "notion"),
      echoServer: app(own, "echo-server"),
    };
  };
  // Kept as a promise's outcome, so a case that never touches apps runs on a server without them.
  const apps = await loadApps().then(
    (loaded) => ({ loaded }),
    (error: unknown) => ({ error }),
  );
  return {
    organization,
    otherOrganization,
    workspaces: { acme, acmeSandbox, qaPlayground },
    get apps() {
      if ("error" in apps) throw apps.error;
      return apps.loaded;
    },
  };
}
