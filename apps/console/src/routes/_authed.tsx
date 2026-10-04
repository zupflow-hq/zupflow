import { createFileRoute, Outlet, redirect } from "@tanstack/react-router";
import { session } from "@/features/auth/session";
import { viewerQuery } from "@/features/queries";

export const Route = createFileRoute("/_authed")({
  beforeLoad: async ({ context, location }) => {
    if (!session.isSignedIn()) {
      throw redirect({ to: "/sign-in", search: { redirect: location.href } });
    }
    await context.queryClient.ensureQueryData(viewerQuery);
  },
  component: Outlet,
});
