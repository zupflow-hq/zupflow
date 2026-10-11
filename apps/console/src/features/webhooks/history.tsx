import {
  type Organization,
  WEBHOOK_DELIVERY_STATES,
  WEBHOOK_DELIVERY_TYPES,
  type WebhookDeliveryAttempt,
  type WebhookDeliveryDetail,
  type WebhookDeliveryErrorClass,
  type WebhookDeliveryId,
  type WebhookDeliveryListQuery,
  type WebhookDeliveryState,
  type WebhookDeliverySummary,
  type WebhookEndpoint,
} from "@zupflow/sdk";
import { ChevronRightIcon, RefreshCwIcon, SendIcon, WebhookIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { CodeTabs } from "@/components/app/code-block";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { EnvBadge } from "@/components/app/environment";
import { MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import {
  useResendWebhookDelivery,
  useWebhookDeliveries,
  useWebhookDelivery,
  useWebhookDeliveryAttempts,
} from "@/features/queries";
import { WorkspacePicker } from "@/features/workspaces/workspace-picker";
import { toastError } from "@/lib/errors";
import { fullDateTime, shortId } from "@/lib/format";

type Filters = Omit<WebhookDeliveryListQuery, "cursor" | "limit">;

const STATES = {
  pending: { label: "Pending", tone: "warning" },
  succeeded: { label: "Succeeded", tone: "success" },
  failed: { label: "Failed", tone: "danger" },
  skipped: { label: "Skipped", tone: "neutral" },
} as const;

const ERRORS: Record<WebhookDeliveryErrorClass, string> = {
  status: "HTTP error",
  refused: "Destination refused",
  redirected: "Redirect refused",
  timeout: "Timed out",
  too_large: "Response exceeded the limit",
  unreachable: "Endpoint unreachable",
  failed: "Delivery failed",
  store_unavailable: "Zupflow temporarily unavailable",
  secret_unavailable: "Signing temporarily unavailable",
};

function DeliveryStatus({ state }: { state: WebhookDeliveryState }) {
  return <StatusPill tone={STATES[state].tone}>{STATES[state].label}</StatusPill>;
}

function result(status: number | null, error: WebhookDeliveryErrorClass | null) {
  if (status !== null) return `HTTP ${status}`;
  return error ? ERRORS[error] : "No response recorded";
}

export function WebhookHistory({
  org,
  endpoint,
  filters,
  onFiltersChange,
  selected,
  onSelect,
}: {
  org: Organization;
  endpoint: WebhookEndpoint;
  filters: Filters;
  onFiltersChange: (filters: Filters) => void;
  selected?: WebhookDeliveryId;
  onSelect: (id: WebhookDeliveryId | undefined) => void;
}) {
  const deliveries = useWebhookDeliveries(org.id, endpoint.id, endpoint.environment, filters);
  const detail = useWebhookDelivery(org.id, endpoint.id, endpoint.environment, selected);
  const attempts = useWebhookDeliveryAttempts(
    org.id,
    endpoint.id,
    endpoint.environment,
    selected,
    detail.data?.state === "pending",
  );
  const items = deliveries.data?.pages.flatMap((page) => page.items) ?? [];
  const total = deliveries.data?.pages[0]?.pagination.total ?? 0;

  return (
    <PageContainer>
      <PageHeader
        title="Delivery history"
        description={<span className="break-all font-mono text-xs">{endpoint.url}</span>}
        eyebrow={
          <>
            <EnvBadge environment={endpoint.environment} />
            <StatusPill tone={endpoint.enabled ? "success" : "neutral"}>
              {endpoint.enabled ? "Enabled" : "Disabled"}
            </StatusPill>
          </>
        }
        actions={
          <Button variant="outline" onClick={() => deliveries.refetch()} disabled={deliveries.isFetching}>
            {deliveries.isFetching ? <Spinner /> : <RefreshCwIcon />}
            Refresh
          </Button>
        }
      />
      <Toolbar className="sm:flex-wrap sm:justify-start">
        <WorkspacePicker
          orgId={org.id}
          environment={endpoint.environment}
          value={filters.workspace_id}
          onChange={(workspace_id) => onFiltersChange({ ...filters, workspace_id })}
          allLabel="All workspaces and test events"
        />
        <Select
          value={filters.state ?? "all"}
          onValueChange={(value) =>
            onFiltersChange({ ...filters, state: value === "all" ? undefined : (value as WebhookDeliveryState) })
          }
        >
          <SelectTrigger aria-label="Delivery state" className="w-full bg-card sm:w-40">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All states</SelectItem>
            {WEBHOOK_DELIVERY_STATES.map((state) => (
              <SelectItem key={state} value={state}>
                {STATES[state].label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select
          value={filters.event_type ?? "all"}
          onValueChange={(value) =>
            onFiltersChange({ ...filters, event_type: value === "all" ? undefined : (value as Filters["event_type"]) })
          }
        >
          <SelectTrigger aria-label="Event type" className="w-full bg-card sm:w-64">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All event types</SelectItem>
            {WEBHOOK_DELIVERY_TYPES.map((type) => (
              <SelectItem key={type} value={type}>
                {type}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Toolbar>
      <Panel>
        {deliveries.isError ? (
          <QueryError
            error={deliveries.error}
            onRetry={() => deliveries.refetch()}
            what="delivery history"
            className="m-3"
          />
        ) : deliveries.isPending ? (
          <RowsSkeleton rows={6} columns={[45, 20, 15]} />
        ) : items.length === 0 ? (
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <WebhookIcon />
              </EmptyMedia>
              <EmptyTitle>No deliveries for these filters</EmptyTitle>
              <EmptyDescription>
                Change the filters, or send a test event from Webhooks to check this endpoint.
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        ) : (
          <ul className="divide-y">
            {items.map((item) => (
              <DeliveryRow key={item.id} delivery={item} onSelect={() => onSelect(item.id)} />
            ))}
          </ul>
        )}
      </Panel>
      {items.length > 0 ? (
        <p className="mt-3 text-xs text-muted-foreground">
          Showing {items.length.toLocaleString("en")} of {total.toLocaleString("en")} deliveries
        </p>
      ) : null}
      {deliveries.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button variant="outline" disabled={deliveries.isFetchingNextPage} onClick={() => deliveries.fetchNextPage()}>
            {deliveries.isFetchingNextPage ? <Spinner /> : null}Load older deliveries
          </Button>
        </div>
      ) : null}

      <Dialog open={selected !== undefined} onOpenChange={(open) => !open && onSelect(undefined)}>
        <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-3xl">
          <DialogHeader>
            <DialogTitle>Delivery details</DialogTitle>
            <DialogDescription>Recorded attempts and the event sent to this endpoint.</DialogDescription>
          </DialogHeader>
          {detail.isError ? (
            <QueryError error={detail.error} onRetry={() => detail.refetch()} what="this delivery" />
          ) : !detail.data ? (
            <RowsSkeleton rows={5} columns={[25, 55]} />
          ) : (
            <>
              <DeliveryDetails delivery={detail.data} />
              {canAdminister(org) ? (
                <ResendDelivery
                  key={detail.data.id}
                  org={org}
                  endpoint={endpoint}
                  delivery={detail.data}
                  onResent={onSelect}
                />
              ) : null}
              <section className="min-w-0">
                <SectionHeader
                  title="Attempts"
                  description={`${detail.data.attempts} recorded ${detail.data.attempts === 1 ? "attempt" : "attempts"}`}
                />
                {!detail.data.attempt_history_complete ? (
                  <p role="note" className="mb-3 rounded-lg bg-warning-soft p-3 text-sm">
                    Some older attempts have only a summary. Their individual results and durations were not recorded.
                  </p>
                ) : null}
                <Panel>
                  {attempts.isError ? (
                    <QueryError
                      error={attempts.error}
                      onRetry={() => attempts.refetch()}
                      what="delivery attempts"
                      className="m-3"
                    />
                  ) : attempts.isPending ? (
                    <RowsSkeleton rows={3} columns={[15, 40, 25]} />
                  ) : attempts.data?.pages[0]?.items.length === 0 ? (
                    <p className="p-4 text-sm text-muted-foreground">No individual attempts recorded.</p>
                  ) : (
                    <ol className="divide-y">
                      {attempts.data?.pages
                        .flatMap((page) => page.items)
                        .map((attempt) => (
                          <AttemptRow key={attempt.number} attempt={attempt} />
                        ))}
                    </ol>
                  )}
                </Panel>
                {attempts.hasNextPage ? (
                  <Button
                    variant="outline"
                    className="mt-3"
                    disabled={attempts.isFetchingNextPage}
                    onClick={() => attempts.fetchNextPage()}
                  >
                    {attempts.isFetchingNextPage ? <Spinner /> : null}Load more attempts
                  </Button>
                ) : null}
              </section>
              <section className="min-w-0">
                <SectionHeader
                  title="Event"
                  description="Deduplicate by event ID, which stays the same across attempts and resends."
                />
                <CodeTabs samples={[{ label: "JSON", code: JSON.stringify(detail.data.event, null, 2) }]} />
              </section>
            </>
          )}
        </DialogContent>
      </Dialog>
    </PageContainer>
  );
}

function DeliveryRow({ delivery, onSelect }: { delivery: WebhookDeliverySummary; onSelect: () => void }) {
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        aria-label={`View ${delivery.event_type} delivery ${shortId(delivery.id)}`}
        className="flex w-full items-center gap-3 rounded-lg p-4 text-left outline-none hover:bg-muted/40 focus-visible:ring-2 focus-visible:ring-ring"
      >
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className="flex flex-wrap items-center gap-2">
            <span className="truncate font-mono text-[12px] font-medium">{delivery.event_type}</span>
            <DeliveryStatus state={delivery.state} />
          </div>
          <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
            <code>{shortId(delivery.event_id)}</code>
            <span className="max-w-full truncate">
              {delivery.workspace ? (delivery.workspace.external_id ?? shortId(delivery.workspace.id)) : "Test event"}
            </span>
            <span>
              {delivery.attempts} {delivery.attempts === 1 ? "attempt" : "attempts"}
            </span>
            {delivery.last_attempt_at ? <span>{result(delivery.last_status_code, delivery.last_error)}</span> : null}
          </p>
          {delivery.next_attempt_at ? (
            <p className="text-xs text-muted-foreground">
              {delivery.attempts === 0
                ? "First attempt "
                : Date.parse(delivery.next_attempt_at) <= Date.now()
                  ? "Retry due · "
                  : "Next retry "}
              <RelativeTime iso={delivery.next_attempt_at} />
            </p>
          ) : null}
        </div>
        <span className="hidden text-xs text-muted-foreground sm:block">
          <RelativeTime iso={delivery.created_at} />
        </span>
        <ChevronRightIcon aria-hidden className="size-4 shrink-0 text-muted-foreground" />
      </button>
    </li>
  );
}

function DeliveryDetails({ delivery }: { delivery: WebhookDeliveryDetail }) {
  return (
    <Panel>
      <Properties>
        <Property label="State">
          <DeliveryStatus state={delivery.state} />
        </Property>
        <Property label="Delivery ID">
          <MonoValue value={delivery.id} display={shortId(delivery.id)} />
        </Property>
        <Property label="Event ID">
          <MonoValue value={delivery.event_id} display={shortId(delivery.event_id)} />
        </Property>
        <Property label="Event type">
          <code className="break-all text-xs">{delivery.event_type}</code>
        </Property>
        <Property label="Workspace">
          {delivery.workspace ? (
            <div className="space-y-1">
              <MonoValue value={delivery.workspace.id} display={shortId(delivery.workspace.id)} />
              {delivery.workspace.external_id ? (
                <p className="break-all text-muted-foreground">{delivery.workspace.external_id}</p>
              ) : null}
            </div>
          ) : (
            "None · test event"
          )}
        </Property>
        <Property label="Created">
          <RelativeTime iso={delivery.created_at} />{" "}
          <span className="text-muted-foreground">· {fullDateTime(delivery.created_at)}</span>
        </Property>
        <Property label="Latest result">
          {delivery.last_attempt_at ? result(delivery.last_status_code, delivery.last_error) : "No attempt recorded"}
        </Property>
        <Property label="Next attempt">
          {delivery.next_attempt_at ? (
            <>
              <RelativeTime iso={delivery.next_attempt_at} />{" "}
              <span className="text-muted-foreground">· {fullDateTime(delivery.next_attempt_at)}</span>
            </>
          ) : (
            "None scheduled"
          )}
        </Property>
      </Properties>
    </Panel>
  );
}

function ResendDelivery({
  org,
  endpoint,
  delivery,
  onResent,
}: {
  org: Organization;
  endpoint: WebhookEndpoint;
  delivery: WebhookDeliveryDetail;
  onResent: (id: WebhookDeliveryId) => void;
}) {
  const resend = useResendWebhookDelivery(org.id, endpoint.id);
  const [confirming, setConfirming] = useState(false);
  const reason =
    delivery.state === "pending"
      ? "This delivery is still pending. Wait until it finishes before resending."
      : !endpoint.enabled
        ? "Enable this webhook endpoint before resending."
        : delivery.event_type !== "webhook.test" && !endpoint.events.includes(delivery.event_type)
          ? "Subscribe this endpoint to the event type before resending."
          : null;
  return (
    <div className="space-y-2">
      <Button variant="outline" disabled={reason !== null || resend.isPending} onClick={() => setConfirming(true)}>
        <SendIcon />
        Resend event
      </Button>
      {reason ? <p className="text-sm text-muted-foreground">{reason}</p> : null}
      <ConfirmDialog
        open={confirming}
        onOpenChange={(open) => !resend.isPending && setConfirming(open)}
        title="Resend this event?"
        description={
          <>
            <p>
              Send this event again to <span className="break-all font-mono text-xs">{endpoint.url}</span>.
            </p>
            <p>
              It keeps the same event ID and body, with a new delivery and fresh retry attempts. The original history
              stays intact. Your endpoint may ignore an event it has already handled.
            </p>
          </>
        }
        actionLabel="Resend event"
        destructive={false}
        pending={resend.isPending}
        onConfirm={() => {
          if (resend.isPending) return;
          if (reason) {
            setConfirming(false);
            toast.error("Couldn't resend event", { description: reason });
            return;
          }
          resend.mutate(delivery.id, {
            onSuccess: (created) => {
              setConfirming(false);
              toast.success("Event queued for resend");
              onResent(created.id);
            },
            onError: (error) => toastError(error, "Couldn't resend event"),
          });
        }}
      />
    </div>
  );
}

function AttemptRow({ attempt }: { attempt: WebhookDeliveryAttempt }) {
  return (
    <li className="flex flex-wrap items-start gap-x-3 gap-y-1 p-3 text-sm">
      <span className="font-medium">#{attempt.number}</span>
      <div className="min-w-0 flex-1">
        <p>{result(attempt.status_code, attempt.error_class)}</p>
        <p className="text-xs text-muted-foreground">{fullDateTime(attempt.attempted_at)}</p>
      </div>
      <span className="text-xs text-muted-foreground">
        {attempt.duration_ms === null ? "Duration not recorded" : `${attempt.duration_ms.toLocaleString("en")} ms`}
      </span>
    </li>
  );
}
