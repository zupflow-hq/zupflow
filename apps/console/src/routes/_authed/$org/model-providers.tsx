import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/model-providers")({
  staticData: { crumb: "Model providers" },
  component: Outlet,
});
