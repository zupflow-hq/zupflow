import { createFileRoute, redirect } from "@tanstack/react-router";
import { session } from "@/features/auth/session";
import { viewerQuery } from "@/features/queries";
import { lastOrgSlug } from "@/lib/recent";

export const Route = createFileRoute("/")({
  beforeLoad: async ({ context }) => {
    if (!session.isSignedIn()) throw redirect({ to: "/sign-in" });
    const viewer = await context.queryClient.ensureQueryData(viewerQuery);
    const remembered = viewer.organizations.find((o) => o.slug === lastOrgSlug());
    const org = remembered ?? viewer.organizations[0];
    if (!org) throw redirect({ to: "/onboarding" });
    throw redirect({ to: "/$org", params: { org: org.slug } });
  },
});
