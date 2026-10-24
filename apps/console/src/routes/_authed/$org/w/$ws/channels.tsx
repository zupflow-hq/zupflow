import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/w/$ws/channels")({
  staticData: { crumb: "Channels" },
  component: Outlet,
});
