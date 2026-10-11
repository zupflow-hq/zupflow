import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, Link } from "@tanstack/react-router";
import { type Environment, type OrganizationId, WEBHOOK_EVENT_TYPES, type WebhookEndpoint } from "@zupflow/sdk";
import { EllipsisIcon, PencilIcon, PlusIcon, SendIcon, Trash2Icon, WebhookIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { SecretRevealDialog } from "@/components/app/secret-reveal";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useCreateWebhook, useDeleteWebhook, useTestWebhook, useUpdateWebhook, useWebhooks } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/webhooks/")({
  component: WebhooksPage,
});

function WebhooksPage() {
  const org = useOrg();
  useDocumentTitle("Webhooks", org.name);
  const hooks = useWebhooks(org.id);
  const update = useUpdateWebhook(org.id);
  const test = useTestWebhook(org.id);
  const remove = useDeleteWebhook(org.id);
  const admin = canAdminister(org);
  const [editing, setEditing] = useState<WebhookEndpoint | "new" | null>(null);
  const [secret, setSecret] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<WebhookEndpoint | null>(null);
  const environment = useEnvironment();
  const endpoints = hooks.data?.filter((h) => h.environment === environment);

  return (
    <PageContainer>
      <PageHeader
        title="Webhooks"
        description={`Signed, retried deliveries of ${ENVIRONMENT_LABEL[environment]} events to your backend. Deduplicate by event ID and fetch current state after each event; order isn't guaranteed.`}
        actions={
          admin ? (
            <Button onClick={() => setEditing("new")}>
              <PlusIcon />
              Add endpoint
            </Button>
          ) : null
        }
      />
      {hooks.isError ? (
        <QueryError error={hooks.error} onRetry={() => hooks.refetch()} what="webhook endpoints" />
      ) : endpoints?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <WebhookIcon />
              </EmptyMedia>
              <EmptyTitle>No {ENVIRONMENT_LABEL[environment]} endpoints</EmptyTitle>
              <EmptyDescription>Get told when a Connect session completes or a run needs approval.</EmptyDescription>
            </EmptyHeader>
            {admin ? (
              <EmptyContent>
                <Button onClick={() => setEditing("new")}>
                  <PlusIcon />
                  Add endpoint
                </Button>
              </EmptyContent>
            ) : null}
          </Empty>
        </Panel>
      ) : (
        <Panel>
          {!endpoints ? (
            <RowsSkeleton rows={2} />
          ) : (
            <ul className="divide-y">
              {endpoints.map((h) => (
                <li key={h.id} className="flex flex-col gap-3 p-4 sm:flex-row sm:items-center">
                  <div className="min-w-0 flex-1 space-y-1.5">
                    <Link
                      to="/$org/webhooks/$hookId"
                      params={{ org: org.slug, hookId: h.id }}
                      className="block truncate font-mono text-sm font-medium hover:underline"
                    >
                      {h.url}
                    </Link>
                    <p className="text-sm text-muted-foreground">
                      {h.description || "No description"} · {h.events.length}{" "}
                      {h.events.length === 1 ? "event" : "events"} · secret ending{" "}
                      <code className="text-xs">{h.signing_secret_last4}</code>
                    </p>
                  </div>
                  <div className="flex flex-wrap items-center gap-3">
                    {h.last_delivery ? (
                      <span className="flex items-center gap-2 text-xs text-muted-foreground">
                        <StatusPill
                          tone={
                            h.last_delivery.status_code >= 200 && h.last_delivery.status_code < 300
                              ? "success"
                              : "danger"
                          }
                        >
                          {h.last_delivery.status_code || "No response"}
                        </StatusPill>
                        <RelativeTime iso={h.last_delivery.delivered_at} />
                      </span>
                    ) : (
                      <span className="text-xs text-muted-foreground">No deliveries</span>
                    )}
                    <Button asChild variant="outline" size="sm">
                      <Link to="/$org/webhooks/$hookId" params={{ org: org.slug, hookId: h.id }}>
                        View deliveries
                      </Link>
                    </Button>
                    <Switch
                      aria-label={h.enabled ? "Disable endpoint" : "Enable endpoint"}
                      checked={h.enabled}
                      disabled={!admin}
                      onCheckedChange={(enabled) =>
                        update.mutate(
                          { id: h.id, enabled },
                          {
                            onSuccess: () => toast.success(enabled ? "Endpoint enabled" : "Endpoint disabled"),
                            onError: (e) => toastError(e),
                          },
                        )
                      }
                    />
                    {/* Sending a test delivery is part of managing endpoints, like editing them: admins only. */}
                    {admin ? (
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button variant="ghost" size="icon-sm" aria-label="Endpoint actions">
                            <EllipsisIcon />
                          </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                          <DropdownMenuItem
                            onSelect={() =>
                              test.mutate(h.id, {
                                onSuccess: (d) =>
                                  d.status_code >= 200 && d.status_code < 300
                                    ? toast.success(`Test event delivered (${d.status_code})`)
                                    : toast.error(
                                        d.status_code
                                          ? `Your endpoint answered ${d.status_code}`
                                          : "No response from your endpoint",
                                        {
                                          description: "Open delivery history for the result and failure details.",
                                        },
                                      ),
                                onError: (e) => toastError(e),
                              })
                            }
                          >
                            <SendIcon />
                            Send test event
                          </DropdownMenuItem>
                          <DropdownMenuItem onSelect={() => setEditing(h)}>
                            <PencilIcon />
                            Edit
                          </DropdownMenuItem>
                          <DropdownMenuSeparator />
                          <DropdownMenuItem variant="destructive" onSelect={() => setDeleting(h)}>
                            <Trash2Icon />
                            Delete
                          </DropdownMenuItem>
                        </DropdownMenuContent>
                      </DropdownMenu>
                    ) : null}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </Panel>
      )}

      <EndpointDialog
        orgId={org.id}
        environment={environment}
        endpoint={editing}
        onClose={() => setEditing(null)}
        onCreated={(s) => {
          setEditing(null);
          setSecret(s);
        }}
      />
      <SecretRevealDialog
        title="Signing secret"
        description="Verify every delivery's signature with this secret before acting on it."
        secret={secret}
        onClose={() => setSecret(null)}
      />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => !o && setDeleting(null)}
        title="Delete this endpoint?"
        description={<p>Deliveries to {deleting?.url} stop immediately, including pending retries.</p>}
        actionLabel="Delete endpoint"
        pending={remove.isPending}
        onConfirm={() =>
          deleting &&
          remove.mutate(deleting.id, {
            onSuccess: () => {
              toast.success("Endpoint deleted");
              setDeleting(null);
            },
            onError: (e) => toastError(e),
          })
        }
      />
    </PageContainer>
  );
}

const schema = z.object({
  url: z.string().trim().url("Enter a full https:// URL."),
  description: z.string().trim().max(200),
  events: z.array(z.enum(WEBHOOK_EVENT_TYPES)).min(1, "Choose at least one event."),
});
type Values = z.infer<typeof schema>;

function EndpointDialog({
  orgId,
  environment,
  endpoint,
  onClose,
  onCreated,
}: {
  orgId: OrganizationId;
  environment: Environment;
  endpoint: WebhookEndpoint | "new" | null;
  onClose: () => void;
  onCreated: (secret: string) => void;
}) {
  const create = useCreateWebhook(orgId);
  const update = useUpdateWebhook(orgId);
  const existing = endpoint !== "new" ? endpoint : null;
  const form = useForm<Values>({ resolver: zodResolver(schema) });

  useEffect(() => {
    if (endpoint === null) return;
    form.reset(
      existing
        ? {
            url: existing.url,
            description: existing.description,
            events: existing.events,
          }
        : {
            url: "",
            description: "",
            events: ["connect_session.completed", "run.completed", "run.failed"],
          },
    );
  }, [endpoint, existing, form]);

  const pending = create.isPending || update.isPending;
  return (
    <Dialog open={endpoint !== null} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{existing ? "Edit endpoint" : `Add ${ENVIRONMENT_LABEL[environment]} endpoint`}</DialogTitle>
          <DialogDescription>
            Receives {ENVIRONMENT_LABEL[environment]} events only. Deliveries are signed with a secret unique to this
            endpoint.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="endpoint-form"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) => {
              const onError = (e: unknown) => {
                if (!applyFieldErrors(form, e)) toastError(e, "Couldn't save the endpoint");
              };
              if (existing) {
                update.mutate(
                  { id: existing.id, ...values },
                  {
                    onSuccess: () => {
                      toast.success("Endpoint saved");
                      onClose();
                    },
                    onError,
                  },
                );
              } else {
                create.mutate({ ...values, environment }, { onSuccess: (r) => onCreated(r.signing_secret), onError });
              }
            })}
          >
            <FormField
              control={form.control}
              name="url"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Endpoint URL</FormLabel>
                  <FormControl>
                    <Input
                      className="font-mono text-sm"
                      placeholder="https://api.example.com/hooks/zupflow"
                      {...field}
                    />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="description"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>
                    Description <span className="font-normal text-muted-foreground">(optional)</span>
                  </FormLabel>
                  <FormControl>
                    <Input {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="events"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Events</FormLabel>
                  <div className="grid gap-x-4 gap-y-2 rounded-lg border p-3 sm:grid-cols-2">
                    {WEBHOOK_EVENT_TYPES.map((type) => (
                      <div key={type} className="flex items-center gap-2">
                        <Checkbox
                          id={`event-${type}`}
                          checked={field.value?.includes(type)}
                          onCheckedChange={(on) =>
                            field.onChange(
                              on ? [...(field.value ?? []), type] : (field.value ?? []).filter((t) => t !== type),
                            )
                          }
                        />
                        <Label htmlFor={`event-${type}`} className="font-mono text-[12px] font-normal">
                          {type}
                        </Label>
                      </div>
                    ))}
                  </div>
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" form="endpoint-form" disabled={pending}>
            {pending ? <Spinner /> : null}
            {existing ? "Save endpoint" : "Add endpoint"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
