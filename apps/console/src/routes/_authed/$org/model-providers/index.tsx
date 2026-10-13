import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { MODEL_PROVIDERS } from "@zupflow/sdk";
import { PlusIcon, SparklesIcon } from "lucide-react";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { AddConnectionDialog, ConnectionStatusPill } from "@/features/models/connection-dialogs";
import { DefaultModelPanel } from "@/features/models/default-model";
import { useModelConnections } from "@/features/queries";

export const Route = createFileRoute("/_authed/$org/model-providers/")({
  validateSearch: z.object({ add: z.boolean().optional() }),
  component: ModelProvidersPage,
});

function ModelProvidersPage() {
  const org = useOrg();
  useDocumentTitle("Model providers", org.name);
  const environment = useEnvironment();
  const env = ENVIRONMENT_LABEL[environment];
  const navigate = useNavigate({ from: Route.fullPath });
  const { add } = Route.useSearch();
  const setAdding = (open: boolean) => navigate({ search: (s) => ({ ...s, add: open || undefined }), replace: true });
  const connections = useModelConnections(org.id, environment);
  const admin = canAdminister(org);

  return (
    <PageContainer>
      <PageHeader
        title="Model providers"
        description={`Agents run on models from your own provider accounts, with your keys. These are ${env} connections: the other environment keeps its own keys and models.`}
        actions={
          admin ? (
            <Button onClick={() => setAdding(true)}>
              <PlusIcon />
              Connect provider
            </Button>
          ) : null
        }
      />
      <div className="space-y-8">
        <DefaultModelPanel org={org} environment={environment} />

        <section>
          <SectionHeader
            title="Connections"
            description={admin ? undefined : "Owners and admins connect providers and choose models."}
          />
          {connections.isError ? (
            <QueryError error={connections.error} onRetry={() => connections.refetch()} what="connections" />
          ) : connections.data?.length === 0 ? (
            <Panel>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <SparklesIcon />
                  </EmptyMedia>
                  <EmptyTitle>No {env} providers yet</EmptyTitle>
                  <EmptyDescription>
                    Connect OpenAI, Anthropic or OpenRouter with an API key, then add the models your agents may use.
                  </EmptyDescription>
                </EmptyHeader>
                {admin ? (
                  <EmptyContent>
                    <Button onClick={() => setAdding(true)}>
                      <PlusIcon />
                      Connect provider
                    </Button>
                  </EmptyContent>
                ) : null}
              </Empty>
            </Panel>
          ) : (
            <Panel>
              {!connections.data ? (
                <RowsSkeleton rows={2} />
              ) : (
                <ul className="divide-y">
                  {connections.data.map((c) => (
                    <li key={c.id}>
                      <Link
                        to="/$org/model-providers/$connectionId"
                        params={{ org: org.slug, connectionId: c.id }}
                        className="flex flex-col gap-2 p-4 transition-colors outline-none hover:bg-accent/40 focus-visible:bg-accent/40 sm:flex-row sm:items-center sm:gap-4"
                      >
                        <div className="min-w-0 flex-1 space-y-1">
                          <p className="truncate text-sm font-medium">{c.label}</p>
                          <p className="truncate text-sm text-muted-foreground">
                            {MODEL_PROVIDERS[c.kind].name} · key ending{" "}
                            <code className="text-xs">{c.api_key.last4}</code>
                          </p>
                        </div>
                        <div className="flex items-center gap-3">
                          <span className="tabular text-sm text-muted-foreground">
                            {c.model_count} {c.model_count === 1 ? "model" : "models"}
                          </span>
                          <ConnectionStatusPill connection={c} />
                        </div>
                      </Link>
                    </li>
                  ))}
                </ul>
              )}
            </Panel>
          )}
        </section>
      </div>
      <AddConnectionDialog
        org={org}
        environment={environment}
        open={Boolean(add) && admin}
        onOpenChange={setAdding}
        onCreated={(connection) =>
          navigate({
            to: "/$org/model-providers/$connectionId",
            params: { org: org.slug, connectionId: connection.id },
            search: { add: true },
          })
        }
      />
    </PageContainer>
  );
}
