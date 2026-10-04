import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import type { Environment } from "@zupflow/sdk";
import { useCallback } from "react";
import { workspaceQuery } from "./queries";

/**
 * The Console works in one environment at a time, like a dashboard's test
 * mode. Organization pages show only that environment's workspaces, runs,
 * keys and settings.
 *
 * It lives in the URL (`?env=sandbox`; absent means Production) so links are
 * unambiguous, and the organization route retains it across navigation. A
 * workspace page always takes its workspace's environment.
 */
export function useEnvironment(): Environment {
  const { ws } = useParams({ strict: false });
  const { env } = useSearch({ strict: false }) as { env?: Environment };
  const workspace = useQuery({ ...workspaceQuery(ws ?? ""), enabled: Boolean(ws) });
  if (ws && workspace.data) return workspace.data.environment;
  return env === "sandbox" ? "sandbox" : "production";
}

export function envSearch(environment: Environment): { env?: Environment } {
  return environment === "sandbox" ? { env: "sandbox" } : { env: undefined };
}

/**
 * Switch environments. On an organization page the page stays and its data
 * changes; inside a workspace (which exists in one environment only) it goes
 * to the workspace list of the other environment.
 */
export function useSwitchEnvironment() {
  const navigate = useNavigate();
  const { org, ws } = useParams({ strict: false });
  return useCallback(
    (next: Environment) => {
      if (!org) return;
      if (ws) {
        void navigate({ to: "/$org/workspaces", params: { org }, search: envSearch(next) });
        return;
      }
      void navigate({ to: ".", search: (prev) => ({ ...prev, ...envSearch(next) }) });
    },
    [navigate, org, ws],
  );
}
