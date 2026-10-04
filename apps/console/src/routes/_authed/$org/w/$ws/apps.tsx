import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/w/$ws/apps")({
  staticData: { crumb: "Workspace apps" },
  component: Outlet,
});
