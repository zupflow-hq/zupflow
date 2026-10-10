import { Link } from "@tanstack/react-router";
import type { AppDefinition, AppToolLevel, Organization, ToolLevel } from "@zupflow/sdk";
import { CheckIcon } from "lucide-react";
import { toast } from "sonner";
import { QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useAppToolLevels, useSetAppToolLevels, useToolPolicy } from "@/features/queries";
import { toastError } from "@/lib/errors";
import { LEVEL_LABEL, levelInForce, NEW_TOOL_LABEL } from "./levels";

const LEVELS: ToolLevel[] = ["allow", "approve", "deny"];
/** The most levels one change sets. */
const MAX_PER_CHANGE = 100;

/**
 * The organization's level for each of a catalog app's tools, in every
 * workspace. A level suggested from a tool's hints isn't in force until an
 * owner or admin confirms it.
 */
export function AppToolLevels({ org, app }: { org: Organization; app: AppDefinition }) {
  const policy = useToolPolicy(org.id);
  const levels = useAppToolLevels(org.id, app.id);
  const set = useSetAppToolLevels(org.id, app.id);
  const admin = canAdminister(org);
  const items = levels.data?.pages.flatMap((page) => page.items) ?? [];
  const suggested = items.filter((l) => !l.confirmed);
  const newTools = policy.data ? NEW_TOOL_LABEL[policy.data.new_tool_level].toLowerCase() : null;

  const save = (changes: { tool: string; level: ToolLevel }[], done: string) =>
    set.mutate(
      { levels: changes },
      {
        onSuccess: () => toast.success(done),
        onError: (e) => toastError(e, "Couldn't save permissions"),
      },
    );

  return (
    <section>
      <SectionHeader
        title="Tool permissions"
        description={
          <>
            What agents may do with each tool, in every workspace; a grant can be stricter, never looser. Suggested
            levels come from a tool's hints and apply once confirmed. Until then, and while a tool's change waits for
            review, the level for new tools applies
            {newTools ? <>: {newTools}</> : null} (
            <Link to="/$org/settings" params={{ org: org.slug }} className="underline underline-offset-2">
              settings
            </Link>
            ).
          </>
        }
        actions={
          admin && suggested.length > 0 ? (
            <Button
              variant="outline"
              size="sm"
              disabled={set.isPending}
              onClick={() =>
                save(
                  suggested.slice(0, MAX_PER_CHANGE).map((l) => ({ tool: l.tool, level: l.level })),
                  "Confirmed suggested levels",
                )
              }
            >
              {set.isPending ? <Spinner /> : <CheckIcon />}
              Confirm suggestions
            </Button>
          ) : null
        }
      />
      <Panel>
        {levels.isPending || policy.isPending ? (
          <RowsSkeleton rows={3} columns={[45, 20]} />
        ) : levels.isError || policy.isError ? (
          <QueryError
            error={levels.error ?? policy.error}
            onRetry={() => {
              void levels.refetch();
              void policy.refetch();
            }}
            what="tool permissions"
            className="m-3"
          />
        ) : items.length === 0 ? (
          <div className="p-6 text-center text-sm">
            <p className="font-medium">No tools yet</p>
            <p className="mt-1 text-muted-foreground">
              A level is suggested for each tool when an installation of {app.name} first lists it.
            </p>
          </div>
        ) : (
          <ul className="divide-y">
            {items.map((level) => (
              <LevelRow
                key={level.tool}
                level={level}
                inForce={levelInForce(level, policy.data.new_tool_level)}
                editable={admin && !set.isPending}
                onChange={(next) =>
                  save([{ tool: level.tool, level: next }], `Set ${level.tool} to ${LEVEL_LABEL[next].toLowerCase()}`)
                }
              />
            ))}
          </ul>
        )}
      </Panel>
      {levels.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button variant="outline" onClick={() => levels.fetchNextPage()} disabled={levels.isFetchingNextPage}>
            {levels.isFetchingNextPage ? <Spinner /> : null}
            Show more tools
          </Button>
        </div>
      ) : null}
      {admin ? null : <p className="mt-3 text-sm text-muted-foreground">Only owners and admins can change these.</p>}
    </section>
  );
}

function LevelRow({
  level,
  inForce,
  editable,
  onChange,
}: {
  level: AppToolLevel;
  inForce: ToolLevel;
  editable: boolean;
  onChange: (level: ToolLevel) => void;
}) {
  return (
    <li className="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:gap-4">
      <div className="min-w-0 flex-1">
        <p className="flex flex-wrap items-center gap-2 text-sm">
          <code className="font-medium break-all">{level.tool}</code>
          {level.destructive ? <StatusPill tone="danger">Destructive hint</StatusPill> : null}
          {level.confirmed ? null : <StatusPill tone="warning">Suggested</StatusPill>}
        </p>
        <p className="mt-0.5 text-xs text-muted-foreground">
          {level.confirmed ? (
            <>
              Set by {level.updated_by?.name ?? "a former member"} <RelativeTime iso={level.updated_at} />
            </>
          ) : (
            <>
              Suggested {LEVEL_LABEL[level.level].toLowerCase()} from its hints. In force:{" "}
              {LEVEL_LABEL[inForce].toLowerCase()}.
            </>
          )}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        {editable && !level.confirmed ? (
          <Button variant="ghost" size="sm" onClick={() => onChange(level.level)}>
            Confirm
          </Button>
        ) : null}
        {editable ? (
          <Select value={level.confirmed ? level.level : ""} onValueChange={(v) => onChange(v as ToolLevel)}>
            <SelectTrigger className="w-full sm:w-44" aria-label={`Level for ${level.tool}`}>
              <SelectValue placeholder="Choose a level" />
            </SelectTrigger>
            <SelectContent>
              {LEVELS.map((l) => (
                <SelectItem key={l} value={l}>
                  {LEVEL_LABEL[l]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        ) : (
          <span className="text-sm">{LEVEL_LABEL[inForce]}</span>
        )}
      </div>
    </li>
  );
}
