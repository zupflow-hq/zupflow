import { zodResolver } from "@hookform/resolvers/zod";
import type { ChannelSettings, Organization } from "@zupflow/sdk";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useChannelSettings, useUpdateChannelSettings } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

const whole = (min: number, max: number, range: string) =>
  z.coerce
    .number<string | number>({ error: "Enter a number." })
    .int("Use a whole number.")
    .min(min, `Use ${range}.`)
    .max(max, `Use ${range}.`);

const schema = z.object({
  breaker_runs: whole(1, 200, "1 to 200"),
  breaker_window_minutes: whole(1, 1440, "1 to 1,440"),
  breaker_pause_minutes: whole(1, 1440, "1 to 1,440"),
  message_retention_days: whole(1, 365, "1 to 365"),
});
type Values = z.input<typeof schema>;

const valuesOf = (s: ChannelSettings): Values => ({
  breaker_runs: s.breaker_runs,
  breaker_window_minutes: s.breaker_window_minutes,
  breaker_pause_minutes: s.breaker_pause_minutes,
  message_retention_days: s.message_retention_days,
});

/**
 * The organization's channel settings, in both environments: when agents
 * pause in a conversation that's running away (most often agents answering
 * one another), and how long channel messages are kept.
 */
export function ChannelSettingsSection({ org }: { org: Organization }) {
  const settings = useChannelSettings(org.id);
  return (
    <section>
      <SectionHeader
        title="Channels"
        description="Applies to every workspace's chat conversations, in Sandbox and Production."
      />
      <Panel>
        {settings.isPending ? (
          <RowsSkeleton rows={3} columns={[50, 20]} />
        ) : settings.isError ? (
          <QueryError
            error={settings.error}
            onRetry={() => settings.refetch()}
            what="channel settings"
            className="m-3"
          />
        ) : (
          <SettingsForm org={org} settings={settings.data} />
        )}
      </Panel>
    </section>
  );
}

function SettingsForm({ org, settings }: { org: Organization; settings: ChannelSettings }) {
  const update = useUpdateChannelSettings(org.id);
  const admin = canAdminister(org);
  const form = useForm<Values, unknown, z.output<typeof schema>>({
    resolver: zodResolver(schema),
    values: valuesOf(settings),
  });
  const number = (name: keyof Values, unit: string) => (
    <FormField
      control={form.control}
      name={name}
      render={({ field }) => (
        <FormControl>
          <InputGroup className="w-full sm:w-36">
            <InputGroupInput type="number" inputMode="numeric" className="tabular" {...field} />
            <InputGroupAddon align="inline-end">
              <InputGroupText>{unit}</InputGroupText>
            </InputGroupAddon>
          </InputGroup>
        </FormControl>
      )}
    />
  );

  return (
    <Form {...form}>
      <form
        className="space-y-6 p-5"
        onSubmit={form.handleSubmit((values) =>
          update.mutate(values, {
            onSuccess: () => toast.success("Saved channel settings"),
            onError: (e) => {
              if (!applyFieldErrors(form, e)) toastError(e, "Couldn't save");
            },
          }),
        )}
      >
        <fieldset disabled={!admin} className="space-y-6">
          <div className="space-y-2">
            <p className="text-sm font-medium">Pause runaway conversations</p>
            <p className="text-sm text-muted-foreground">
              When a conversation starts too many runs in a short time, usually agents answering one another, agents
              pause there. Your webhook gets <code className="text-xs">channel_conversation.paused</code>.
            </p>
            <div className="flex flex-wrap items-center gap-x-2 gap-y-2 text-sm">
              <span>After</span>
              {number("breaker_runs", "runs")}
              <span>within</span>
              {number("breaker_window_minutes", "min")}
              <span>pause for</span>
              {number("breaker_pause_minutes", "min")}
            </div>
            {(["breaker_runs", "breaker_window_minutes", "breaker_pause_minutes"] as const).map((name) => {
              const message = form.formState.errors[name]?.message;
              return message ? (
                <p key={name} role="alert" className="text-sm text-destructive">
                  {message}
                </p>
              ) : null;
            })}
          </div>
          <FormField
            control={form.control}
            name="message_retention_days"
            render={({ field }) => (
              <FormItem>
                <FormLabel>Keep messages for</FormLabel>
                <FormControl>
                  <InputGroup className="w-full sm:w-36">
                    <InputGroupInput type="number" inputMode="numeric" className="tabular" {...field} />
                    <InputGroupAddon align="inline-end">
                      <InputGroupText>days</InputGroupText>
                    </InputGroupAddon>
                  </InputGroup>
                </FormControl>
                <FormDescription>
                  Messages older than this are deleted from Zupflow. The chat provider keeps its own copy.
                </FormDescription>
                <FormMessage />
              </FormItem>
            )}
          />
        </fieldset>
        {admin ? (
          <Button type="submit" disabled={!form.formState.isDirty || update.isPending}>
            {update.isPending ? <Spinner /> : null}
            Save
          </Button>
        ) : (
          <p className="text-sm text-muted-foreground">Only owners and admins can change these.</p>
        )}
      </form>
    </Form>
  );
}
