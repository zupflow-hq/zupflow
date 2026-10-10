/**
 * The organization's levels as the Console reads them. A level is in force
 * only once a person confirms it; until then the policy's level for new
 * tools applies. A workspace app's tools have no organization level.
 */
import type { AppId, AppToolLevel, NewToolLevel, Organization, ToolLevel } from "@zupflow/sdk";
import { useEffect, useMemo } from "react";
import { useAppToolLevels, useToolPolicy } from "@/features/queries";

export const LEVEL_LABEL: Record<ToolLevel, string> = {
  allow: "Allow",
  approve: "Require approval",
  deny: "Block",
};

export const NEW_TOOL_LABEL: Record<NewToolLevel, string> = {
  approve: "Require approval",
  deny: "Block",
};

/**
 * What happens to calls of a changed tool until its change is accepted, by
 * the policy's level for new tools; `undefined` while the policy loads.
 */
export function changedToolCalls(newToolLevel: NewToolLevel | undefined): string {
  if (newToolLevel === "deny") return "calls are refused";
  if (newToolLevel === "approve") return "calls use the pinned definition and wait for approval";
  return "calls follow your organization's level for new tools";
}

export function levelInForce(level: AppToolLevel | undefined, newToolLevel: NewToolLevel): ToolLevel {
  return level?.confirmed ? level.level : newToolLevel;
}

/**
 * Every level the organization holds for one catalog app, read a page at a
 * time until all are in, and the level in force for a tool by name. `ready`
 * is false until both the policy and every page are read; `levelOf` answers
 * `null` for a workspace app (`appId` undefined), which has no levels.
 */
export function useLevelsInForce(org: Organization, appId: AppId | undefined) {
  const policy = useToolPolicy(org.id);
  const levels = useAppToolLevels(org.id, appId);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = levels;
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) void fetchNextPage();
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);
  const byTool = useMemo(
    () => new Map((levels.data?.pages.flatMap((page) => page.items) ?? []).map((l) => [l.tool, l])),
    [levels.data],
  );
  const newToolLevel = policy.data?.new_tool_level;
  return {
    ready: appId === undefined || (newToolLevel !== undefined && levels.isSuccess && !hasNextPage),
    error: appId === undefined ? null : (policy.error ?? levels.error),
    retry: () => {
      void policy.refetch();
      void levels.refetch();
    },
    levelOf: (tool: string): ToolLevel | null =>
      appId === undefined || newToolLevel === undefined ? null : levelInForce(byTool.get(tool), newToolLevel),
  };
}
