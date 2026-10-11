import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { WEBHOOK_DELIVERY_STATES, WEBHOOK_DELIVERY_TYPES } from "@zupflow/sdk";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty";
import { useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useWebhooks } from "@/features/queries";
import { WebhookHistory } from "@/features/webhooks/history";
import { useDetailCrumb } from "@/lib/crumbs";
import { idParam, idSearch } from "@/lib/route-ids";

export const Route = createFileRoute("/_authed/$org/webhooks/$hookId")({
  params: idParam("hookId", "webhook_endpoint"),
  validateSearch: z.object({
    workspace: idSearch("workspace").optional(),
    state: z.enum(WEBHOOK_DELIVERY_STATES).optional().catch(undefined),
    event: z.enum(WEBHOOK_DELIVERY_TYPES).optional().catch(undefined),
    delivery: idSearch("webhook_delivery").optional(),
  }),
  component: WebhookPage,
});

function WebhookPage() {
  const org = useOrg();
  const environment = useEnvironment();
  const { hookId } = Route.useParams();
  const { workspace, state, event, delivery } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const endpoints = useWebhooks(org.id);
  const endpoint = endpoints.data?.find((item) => item.id === hookId && item.environment === environment);
  useDetailCrumb(endpoint?.description || "Delivery history");
  useDocumentTitle("Delivery history", org.name);

  if (endpoints.isError) {
    return (
      <PageContainer>
        <QueryError error={endpoints.error} onRetry={() => endpoints.refetch()} what="this endpoint" />
      </PageContainer>
    );
  }
  if (!endpoint) {
    return (
      <PageContainer>
        <PageHeader title="Delivery history" />
        <Panel>
          {endpoints.isPending ? (
            <RowsSkeleton rows={5} />
          ) : (
            <Empty>
              <EmptyHeader>
                <EmptyTitle>Endpoint not found</EmptyTitle>
                <EmptyDescription>
                  Choose an endpoint in {ENVIRONMENT_LABEL[environment]} from Webhooks.
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          )}
        </Panel>
      </PageContainer>
    );
  }

  return (
    <WebhookHistory
      org={org}
      endpoint={endpoint}
      filters={{ workspace_id: workspace, state, event_type: event }}
      onFiltersChange={(filters) =>
        navigate({
          search: (previous) => ({
            ...previous,
            workspace: filters.workspace_id,
            state: filters.state,
            event: filters.event_type,
            delivery: undefined,
          }),
          replace: true,
        })
      }
      selected={delivery}
      onSelect={(id) => navigate({ search: (previous) => ({ ...previous, delivery: id }), replace: true })}
    />
  );
}
