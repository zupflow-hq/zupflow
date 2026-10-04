import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/apps")({
  staticData: { crumb: "App catalog" },
  component: Outlet,
});
