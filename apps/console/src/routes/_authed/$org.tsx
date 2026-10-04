import { createFileRoute, notFound, Outlet, retainSearchParams } from "@tanstack/react-router";
import { ENVIRONMENTS } from "@zupflow/sdk";
import { z } from "zod";
import { SandboxBanner } from "@/components/app/environment";
import { NotFound } from "@/components/app/not-found";
import { AppSidebar } from "@/components/app/shell/app-sidebar";
import { TopBar } from "@/components/app/shell/top-bar";
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar";
import { useEnvironment } from "@/features/environment";
import { viewerQuery } from "@/features/queries";
import { rememberOrg } from "@/lib/recent";

export const Route = createFileRoute("/_authed/$org")({
  // `env` is the `/sandbox/` path prefix, read in by the router's rewrite;
  // absent means Production. It follows every navigation inside the
  // organization.
  validateSearch: z.object({ env: z.enum(ENVIRONMENTS).optional() }),
  search: { middlewares: [retainSearchParams(["env"])] },
  beforeLoad: async ({ context, params }) => {
    const viewer = await context.queryClient.ensureQueryData(viewerQuery);
    const org = viewer.organizations.find((o) => o.slug === params.org);
    if (!org) throw notFound();
    rememberOrg(org.slug);
    return { orgId: org.id };
  },
  component: OrgLayout,
  notFoundComponent: NotFound,
});

function OrgLayout() {
  const environment = useEnvironment();
  return (
    <SidebarProvider>
      <AppSidebar />
      <SidebarInset className="min-w-0 md:overflow-clip md:border">
        <div className="sticky top-0 z-20">
          {environment === "sandbox" ? <SandboxBanner /> : null}
          <TopBar />
        </div>
        <Outlet />
      </SidebarInset>
    </SidebarProvider>
  );
}
