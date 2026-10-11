import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/_authed/$org/webhooks")({
  staticData: { crumb: "Webhooks" },
  component: Outlet,
});
