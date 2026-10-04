import { createFileRoute, notFound, redirect } from "@tanstack/react-router";
import { NotFound } from "@/components/app/not-found";
import { workspaceQuery } from "@/features/queries";
import { WorkspaceLayout } from "@/features/workspaces/workspace-layout";
import { isStatus } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/w/$ws")({
  staticData: { crumb: "$workspace" },
  beforeLoad: async ({ context, params, location, search }) => {
    let environment: string;
    try {
      const workspace = await context.queryClient.ensureQueryData(workspaceQuery(params.ws));
      // A workspace from another organization is not found here, even if the caller can see it.
      if (workspace.organization_id !== context.orgId) throw notFound();
      environment = workspace.environment;
    } catch (error) {
      if (isStatus(error, 404) || isStatus(error, 400)) throw notFound();
      throw error;
    }
    // Keep the URL's environment in step with the workspace's, so every link
    // out of it (back to the list, say) stays in the right environment.
    const current = (search as { env?: string }).env ?? "production";
    if (current !== environment) {
      // Set env explicitly so the organization route's retained env doesn't
      // re-add the old value (which would redirect forever).
      throw redirect({
        to: location.pathname,
        search: (prev: Record<string, unknown>) => ({
          ...prev,
          env: environment === "sandbox" ? "sandbox" : undefined,
        }),
        replace: true,
      });
    }
  },
  component: WorkspaceLayout,
  notFoundComponent: NotFound,
});
