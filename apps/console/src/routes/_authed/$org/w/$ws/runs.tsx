import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/w/$ws/runs")({
  staticData: { crumb: "Runs" },
  component: Outlet,
});
