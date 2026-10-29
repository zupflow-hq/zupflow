import { Link } from "@tanstack/react-router";
import {
  CHANNEL_PROVIDER_NAMES,
  type ChannelProvider,
  type ChannelProviderSettings,
  type Organization,
} from "@zupflow/sdk";
import { useId, useState } from "react";
import { toast } from "sonner";
import { ChoiceCards } from "@/components/app/choice-cards";
import { AppIcon, QueryError, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useChannelProviderSettings, useUpdateChannelProviderSettings } from "@/features/queries";
import { toastError } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { CHANNEL_PROVIDER_ICON, CHANNEL_PROVIDER_SETUP, noProviderReason } from "./labels";

/** The organization settings' "Chat providers" section, for links to it. */
export const CHAT_PROVIDERS_SECTION = "chat-providers";

type Mode = "every" | "chosen";

/** The choice as edited: every provider, or the ones checked. */
interface Draft {
  mode: Mode;
  chosen: ChannelProvider[];
}

const draftOf = (settings: ChannelProviderSettings): Draft => ({
  mode: settings.allowed_providers === null ? "every" : "chosen",
  chosen: settings.allowed_providers ?? settings.providers.map((p) => p.provider),
});

const sameDraft = (a: Draft, b: Draft) =>
  a.mode === b.mode && (a.mode === "every" || [...a.chosen].sort().join() === [...b.chosen].sort().join());

/**
 * Which chat providers the organization's workspaces may link teams of, in
 * both environments, beside whether this server reaches each. Turning one
 * off keeps the teams already linked working; only new ones are refused.
 */
export function ChannelProvidersSection({ org }: { org: Organization }) {
  const settings = useChannelProviderSettings(org.id);
  return (
    <section id={CHAT_PROVIDERS_SECTION} className="scroll-mt-24">
      <SectionHeader
        title="Chat providers"
        description="Which chat providers your workspaces may link teams of, in Sandbox and Production. Turning one off keeps the teams already linked working; no new ones can be linked."
      />
      <Panel>
        {settings.isPending ? (
          <RowsSkeleton rows={3} columns={[50, 20]} />
        ) : settings.isError ? (
          <QueryError error={settings.error} onRetry={() => settings.refetch()} what="chat providers" className="m-3" />
        ) : (
          // A save, here or elsewhere, starts the form again from what's stored.
          <ProvidersForm key={settings.data.updated_at ?? "unsaved"} org={org} settings={settings.data} />
        )}
      </Panel>
    </section>
  );
}

function ProvidersForm({ org, settings }: { org: Organization; settings: ChannelProviderSettings }) {
  const update = useUpdateChannelProviderSettings(org.id);
  const admin = canAdminister(org);
  const id = useId();
  const saved = draftOf(settings);
  const [draft, setDraft] = useState<Draft>(saved);
  const every = draft.mode === "every";
  const toggle = (provider: ChannelProvider, on: boolean) =>
    setDraft((d) => ({
      ...d,
      chosen: on ? [...new Set([...d.chosen, provider])] : d.chosen.filter((p) => p !== provider),
    }));

  return (
    <div className="space-y-5 p-5">
      <ChoiceCards<Mode>
        label="Allowed chat providers"
        value={draft.mode}
        onValueChange={(mode) => setDraft((d) => ({ ...d, mode }))}
        className="sm:grid-cols-2"
        options={[
          {
            value: "every",
            disabled: !admin,
            content: (
              <>
                <span className="text-sm font-medium">Every provider</span>
                <span className="text-xs text-muted-foreground">Including ones Zupflow adds later.</span>
              </>
            ),
          },
          {
            value: "chosen",
            disabled: !admin,
            content: (
              <>
                <span className="text-sm font-medium">Only the ones you choose</span>
                <span className="text-xs text-muted-foreground">
                  A provider Zupflow adds later stays off until you turn it on.
                </span>
              </>
            ),
          },
        ]}
      />
      <ul className="-mx-5 divide-y border-y sm:mx-0 sm:rounded-lg sm:border">
        {settings.providers.map(({ provider, reachable }) => {
          const checkbox = `${id}-${provider}`;
          const disabled = !admin || every;
          const allowed = every || draft.chosen.includes(provider);
          return (
            <li key={provider} className="flex items-center gap-3 px-4 py-2.5">
              <Checkbox
                id={checkbox}
                checked={allowed}
                disabled={disabled}
                onCheckedChange={(v) => toggle(provider, v === true)}
              />
              <AppIcon icon={CHANNEL_PROVIDER_ICON[provider]} size="sm" />
              <Label
                htmlFor={checkbox}
                className={cn("min-w-0 flex-1 flex-col items-start gap-0.5 font-normal", !disabled && "cursor-pointer")}
              >
                <span className="flex min-w-0 flex-wrap items-center gap-2">
                  <span className="font-medium">{CHANNEL_PROVIDER_NAMES[provider]}</span>
                  {reachable ? null : <StatusPill tone="warning">This server can't reach it</StatusPill>}
                </span>
                {reachable ? null : (
                  <span className="text-xs text-muted-foreground">
                    {allowed
                      ? "Allowed. It's offered once the server reaches it."
                      : "Not allowed, and the server can't reach it yet either."}{" "}
                    {CHANNEL_PROVIDER_SETUP[provider]}
                  </span>
                )}
              </Label>
            </li>
          );
        })}
      </ul>
      {!every && draft.chosen.length === 0 ? (
        <p className="text-sm text-muted-foreground">With none chosen, no workspace can link a new team.</p>
      ) : null}
      {admin ? (
        <Button
          disabled={sameDraft(draft, saved) || update.isPending}
          onClick={() =>
            update.mutate(
              { allowed_providers: every ? null : draft.chosen },
              {
                onSuccess: () => toast.success("Saved chat providers"),
                onError: (e) => toastError(e, "Couldn't save"),
              },
            )
          }
        >
          {update.isPending ? <Spinner /> : null}
          Save
        </Button>
      ) : (
        <p className="text-sm text-muted-foreground">Only owners and admins can change these.</p>
      )}
    </div>
  );
}

/**
 * Why no team can be linked here, for anyone who reads the page: and where
 * the organization's choice is why, who changes it, with a link to it for
 * the owners and admins who can.
 */
export function NoProviderReason({
  org,
  settings,
  linked = false,
}: {
  org: Organization;
  settings: ChannelProviderSettings | undefined;
  linked?: boolean;
}) {
  const { text, choice } = noProviderReason(settings, linked);
  if (!choice) return text;
  return canAdminister(org) ? (
    <>
      {text} You can change this in the organization's{" "}
      <Link
        to="/$org/settings"
        params={{ org: org.slug }}
        hash={CHAT_PROVIDERS_SECTION}
        className="font-medium underline underline-offset-2"
      >
        chat providers
      </Link>
      .
    </>
  ) : (
    `${text} An owner or admin can change this in the organization's settings.`
  );
}
