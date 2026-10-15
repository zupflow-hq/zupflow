import {
  CHANNEL_PROVIDER_NAMES,
  CHANNEL_PROVIDERS,
  type ChannelToolLevel,
  type ChannelToolName,
  type Organization,
  type ToolLevel,
} from "@zupflow/sdk";
import { toast } from "sonner";
import { AppIcon, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { StatusPill, ToolEffectTag } from "@/components/app/status";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { canAdminister } from "@/features/context";
import { useChannelToolLevels, useSetChannelToolLevels } from "@/features/queries";
import { LEVEL_LABEL } from "@/features/tool-policy/levels";
import { toastError } from "@/lib/errors";
import { CHANNEL_PROVIDER_ICON, CHANNEL_TOOL_COPY } from "./labels";

const LEVELS: ToolLevel[] = ["allow", "approve", "deny"];

/**
 * The organization's level for each channel tool, beside each app's: what any
 * agent may do in Slack, in every workspace. Unlike an app's suggested
 * levels, a channel tool's default is Zupflow's and in force until changed.
 */
export function ChannelToolLevels({ org }: { org: Organization }) {
  const levels = useChannelToolLevels(org.id);
  const set = useSetChannelToolLevels(org.id);
  const admin = canAdminister(org);

  return (
    <section>
      <SectionHeader
        title="Channel tool permissions"
        description="What agents may do in Slack, in every workspace. Each agent's channel settings can be stricter, never looser. Sending anywhere but the thread that woke an agent asks first by default, and deleting is blocked."
      />
      {levels.isError ? (
        <QueryError error={levels.error} onRetry={() => levels.refetch()} what="channel tool permissions" />
      ) : (
        <div className="space-y-4">
          {CHANNEL_PROVIDERS.map((provider) => {
            const rows = levels.data?.filter((l) => l.provider === provider) ?? [];
            return (
              <Panel key={provider}>
                <div className="flex items-center gap-3 border-b px-4 py-3">
                  <AppIcon icon={CHANNEL_PROVIDER_ICON[provider]} size="sm" />
                  <p className="font-medium">{CHANNEL_PROVIDER_NAMES[provider]}</p>
                </div>
                {levels.isPending ? (
                  <RowsSkeleton rows={4} columns={[45, 20]} />
                ) : (
                  <ul className="divide-y">
                    {rows.map((level) => (
                      <LevelRow
                        key={level.tool}
                        level={level}
                        editable={admin}
                        // One level saves at a time; the others wait, still showing their controls.
                        disabled={set.isPending}
                        onChange={(next) =>
                          set.mutate(
                            { provider, levels: [{ tool: level.tool, level: next }] },
                            {
                              onSuccess: () =>
                                toast.success(
                                  `Set ${CHANNEL_TOOL_COPY[level.tool as ChannelToolName]?.title ?? level.tool} to ${LEVEL_LABEL[next].toLowerCase()}`,
                                ),
                              onError: (e) => toastError(e, "Couldn't save the level"),
                            },
                          )
                        }
                      />
                    ))}
                  </ul>
                )}
              </Panel>
            );
          })}
        </div>
      )}
      {admin ? null : <p className="mt-3 text-sm text-muted-foreground">Only owners and admins can change these.</p>}
    </section>
  );
}

function LevelRow({
  level,
  editable,
  disabled,
  onChange,
}: {
  level: ChannelToolLevel;
  editable: boolean;
  disabled: boolean;
  onChange: (level: ToolLevel) => void;
}) {
  const copy = CHANNEL_TOOL_COPY[level.tool as ChannelToolName];
  return (
    <li className="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:gap-4">
      <div className="min-w-0 flex-1">
        <p className="flex flex-wrap items-center gap-2 text-sm">
          <span className="font-medium">{copy?.title ?? level.tool}</span>
          <ToolEffectTag effect={level.effect} />
          {level.confirmed ? null : <StatusPill tone="neutral">Default</StatusPill>}
        </p>
        <p className="mt-0.5 text-xs text-muted-foreground">
          {copy?.description}{" "}
          {level.confirmed ? (
            <>
              Set by {level.updated_by?.name ?? "a former member"} <RelativeTime iso={level.updated_at} />.
            </>
          ) : null}
        </p>
      </div>
      {editable ? (
        <Select value={level.level} disabled={disabled} onValueChange={(v) => onChange(v as ToolLevel)}>
          <SelectTrigger className="w-full shrink-0 sm:w-44" aria-label={`Level for ${copy?.title ?? level.tool}`}>
            <SelectValue />
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
        <span className="shrink-0 text-sm">{LEVEL_LABEL[level.level]}</span>
      )}
    </li>
  );
}
