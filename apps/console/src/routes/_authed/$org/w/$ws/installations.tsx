import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/w/$ws/installations")({
  staticData: { crumb: "Installations" },
  component: Outlet,
});
