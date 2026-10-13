import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { MODEL_PROVIDERS, type Model, type ModelConnection, type Organization } from "@zupflow/sdk";
import {
  AlertTriangleIcon,
  EllipsisIcon,
  KeyRoundIcon,
  PencilIcon,
  PlusIcon,
  SparklesIcon,
  StarIcon,
  Trash2Icon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { ENVIRONMENT_LABEL, EnvBadge } from "@/components/app/environment";
import { MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { envSearch, useEnvironment } from "@/features/environment";
import { AddModelsDialog } from "@/features/models/add-models-dialog";
import { ConnectionStatusPill, RenameDialog, ReplaceKeyDialog } from "@/features/models/connection-dialogs";
import { contextWindowLabel } from "@/features/models/labels";
import {
  useDeleteModel,
  useDeleteModelConnection,
  useModelConnection,
  useModelList,
  useRenameModel,
  useSetDefaultModel,
  useUpdateModelConnection,
} from "@/features/queries";
import { useDetailCrumb } from "@/lib/crumbs";
import { isApiError, toastError } from "@/lib/errors";
import { idParam } from "@/lib/route-ids";

export const Route = createFileRoute("/_authed/$org/model-providers/$connectionId")({
  params: idParam("connectionId", "model_connection"),
  validateSearch: z.object({ add: z.boolean().optional() }),
  component: ConnectionPage,
});

type Dialog = "rename" | "key" | "delete" | null;

function ConnectionPage() {
  const org = useOrg();
  const environment = useEnvironment();
  const { connectionId } = Route.useParams();
  const { add } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const setAdding = (open: boolean) => navigate({ search: (s) => ({ ...s, add: open || undefined }), replace: true });
  const connection = useModelConnection(org.id, connectionId);
  const rename = useUpdateModelConnection(org.id, connectionId);
  const remove = useDeleteModelConnection(org.id);
  const [dialog, setDialog] = useState<Dialog>(null);
  const admin = canAdminister(org);
  const c = connection.data;
  useDetailCrumb(c?.label);
  useDocumentTitle(c?.label, "Model providers", org.name);

  if (connection.isError) {
    return (
      <PageContainer>
        <QueryError error={connection.error} onRetry={() => connection.refetch()} what="this connection" />
      </PageContainer>
    );
  }
  if (c && c.environment !== environment) {
    // Connections live in one environment; follow it rather than mixing two on one page.
    return (
      <PageContainer>
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyTitle>
                {c.label} is a {ENVIRONMENT_LABEL[c.environment]} connection
              </EmptyTitle>
              <EmptyDescription>
                You're viewing {ENVIRONMENT_LABEL[environment]}. Each environment keeps its own providers and keys.
              </EmptyDescription>
            </EmptyHeader>
            <EmptyContent>
              <Button asChild>
                <Link
                  to="/$org/model-providers/$connectionId"
                  params={{ org: org.slug, connectionId: c.id }}
                  search={envSearch(c.environment)}
                >
                  Open in {ENVIRONMENT_LABEL[c.environment]}
                </Link>
              </Button>
            </EmptyContent>
          </Empty>
        </Panel>
      </PageContainer>
    );
  }

  return (
    <PageContainer>
      <PageHeader
        eyebrow={
          c ? (
            <>
              <EnvBadge environment={c.environment} size="sm" />
              <span className="text-xs text-muted-foreground">{MODEL_PROVIDERS[c.kind].name}</span>
              <ConnectionStatusPill connection={c} />
            </>
          ) : (
            <Skeleton className="h-5 w-40" />
          )
        }
        title={c ? c.label : <Skeleton className="h-8 w-56" />}
        actions={
          c && admin ? (
            <>
              <Button onClick={() => setAdding(true)} disabled={c.status === "invalid_key"}>
                <PlusIcon />
                Add models
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline" size="icon" aria-label="More actions">
                    <EllipsisIcon />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem onSelect={() => setDialog("rename")}>
                    <PencilIcon />
                    Rename
                  </DropdownMenuItem>
                  <DropdownMenuItem onSelect={() => setDialog("key")}>
                    <KeyRoundIcon />
                    Replace key
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem variant="destructive" onSelect={() => setDialog("delete")}>
                    <Trash2Icon />
                    Delete connection
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </>
          ) : null
        }
      />

      {c?.status === "invalid_key" ? (
        <div
          role="alert"
          className="mb-6 flex flex-col items-start gap-3 rounded-xl border border-destructive/30 bg-danger-soft/50 p-4 text-sm sm:flex-row sm:items-center"
        >
          <AlertTriangleIcon className="size-4 shrink-0 text-destructive" />
          <p className="flex-1">
            {MODEL_PROVIDERS[c.kind].name} refused this connection's key. Runs on its models fail until{" "}
            {admin ? "you replace it" : "an admin replaces it"}.
          </p>
          {admin ? (
            <Button variant="outline" size="sm" onClick={() => setDialog("key")}>
              <KeyRoundIcon />
              Replace key
            </Button>
          ) : null}
        </div>
      ) : null}

      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_18rem]">
        <section className="min-w-0">
          <SectionHeader title="Models" />
          {c ? (
            <ConnectionModels org={org} connection={c} admin={admin} onAdd={() => setAdding(true)} />
          ) : (
            <Panel>
              <RowsSkeleton rows={3} />
            </Panel>
          )}
        </section>
        <section>
          <SectionHeader title="Details" />
          <Panel>
            {c ? (
              <Properties>
                <Property label="API key" className="sm:grid-cols-1 sm:gap-1">
                  <span>
                    Ending <code className="text-xs">{c.api_key.last4}</code>
                    <span className="text-muted-foreground">
                      {" "}
                      · version {c.key_version}
                      {c.api_key.updated_at ? (
                        <>
                          , set <RelativeTime iso={c.api_key.updated_at} />
                        </>
                      ) : null}
                    </span>
                  </span>
                </Property>
                <Property label="Connection ID" className="sm:grid-cols-1 sm:gap-1">
                  <MonoValue value={c.id} />
                </Property>
                <Property label="Created" className="sm:grid-cols-1 sm:gap-1">
                  <RelativeTime iso={c.created_at} />
                </Property>
              </Properties>
            ) : (
              <RowsSkeleton rows={3} columns={[60]} />
            )}
          </Panel>
        </section>
      </div>

      {c && admin ? (
        <>
          <AddModelsDialog
            org={org}
            connection={c}
            open={Boolean(add) && c.status === "active"}
            onOpenChange={setAdding}
          />
          <RenameDialog
            title="Rename connection"
            description="Shown with its models wherever they're chosen."
            initial={c.label}
            open={dialog === "rename"}
            onOpenChange={(open) => setDialog(open ? "rename" : null)}
            pending={rename.isPending}
            onSave={(label, setError) =>
              rename.mutate(
                { label },
                {
                  onSuccess: () => {
                    setDialog(null);
                    toast.success("Renamed the connection");
                  },
                  onError: (e) => {
                    if (isApiError(e) && e.fields?.label) setError(e.fields.label);
                    else toastError(e, "Couldn't rename the connection");
                  },
                },
              )
            }
          />
          <ReplaceKeyDialog
            org={org}
            connection={c}
            open={dialog === "key"}
            onOpenChange={(open) => setDialog(open ? "key" : null)}
            onReplaced={(next) =>
              toast.success(`Replaced the key. ${next.label} now uses the key ending ${next.api_key.last4}.`)
            }
          />
          <ConfirmDialog
            open={dialog === "delete"}
            onOpenChange={(open) => setDialog(open ? "delete" : null)}
            title={`Delete ${c.label}?`}
            description={
              <p>
                Its key is deleted and its {c.model_count === 1 ? "model is" : `${c.model_count} models are`} removed.
                While the default model or an agent uses one of them, deleting is refused. This can't be undone.
              </p>
            }
            confirmText={c.label}
            actionLabel="Delete connection"
            pending={remove.isPending}
            onConfirm={() =>
              remove.mutate(c.id, {
                onSuccess: () => {
                  toast.success(`Deleted ${c.label}`);
                  void navigate({ to: "/$org/model-providers", params: { org: org.slug }, search: {} });
                },
                onError: (e) => {
                  setDialog(null);
                  toastError(e, "Couldn't delete the connection");
                },
              })
            }
          />
        </>
      ) : null}
    </PageContainer>
  );
}

function ConnectionModels({
  org,
  connection,
  admin,
  onAdd,
}: {
  org: Organization;
  connection: ModelConnection;
  admin: boolean;
  onAdd: () => void;
}) {
  const models = useModelList(org.id, { environment: connection.environment, connection_id: connection.id, limit: 50 });
  const remove = useDeleteModel(org.id);
  const rename = useRenameModel(org.id);
  const setDefault = useSetDefaultModel(org.id, connection.environment);
  const [removing, setRemoving] = useState<Model | null>(null);
  const [renaming, setRenaming] = useState<Model | null>(null);
  const items = models.data?.pages.flatMap((p) => p.items);

  if (models.isError) {
    return <QueryError error={models.error} onRetry={() => models.refetch()} what="models" />;
  }
  if (items?.length === 0) {
    return (
      <Panel>
        <Empty>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <SparklesIcon />
            </EmptyMedia>
            <EmptyTitle>No models added yet</EmptyTitle>
            <EmptyDescription>
              Search what {MODEL_PROVIDERS[connection.kind].name} lists for this key and add the ones agents may use.
            </EmptyDescription>
          </EmptyHeader>
          {admin && connection.status === "active" ? (
            <EmptyContent>
              <Button onClick={onAdd}>
                <PlusIcon />
                Add models
              </Button>
            </EmptyContent>
          ) : null}
        </Empty>
      </Panel>
    );
  }
  return (
    <Panel className="overflow-hidden">
      {!items ? (
        <RowsSkeleton rows={3} />
      ) : (
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead className="pl-4">Model</TableHead>
              <TableHead className="hidden md:table-cell">Context</TableHead>
              <TableHead className="hidden text-right sm:table-cell">Agents</TableHead>
              <TableHead className="w-12 pr-4">
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {items.map((m) => (
              <TableRow key={m.id}>
                <TableCell className="w-full max-w-0 py-3 pl-4">
                  <div className="flex min-w-0 items-center gap-2">
                    <span className="truncate font-medium">{m.name}</span>
                    {m.is_default ? <StatusPill tone="info">Default</StatusPill> : null}
                    {m.status === "unavailable" ? <StatusPill tone="danger">Unavailable</StatusPill> : null}
                  </div>
                  <p className="truncate font-mono text-xs text-muted-foreground">{m.provider_model_id}</p>
                </TableCell>
                <TableCell className="hidden whitespace-nowrap text-muted-foreground md:table-cell">
                  {contextWindowLabel(m.context_window) ?? "Not listed"}
                </TableCell>
                <TableCell className="tabular hidden text-right sm:table-cell">
                  {m.agent_count === 0 ? <span className="text-muted-foreground">None</span> : m.agent_count}
                </TableCell>
                <TableCell className="pr-4 text-right">
                  {admin ? (
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon" aria-label={`Actions for ${m.name}`}>
                          {setDefault.isPending && setDefault.variables === m.id ? <Spinner /> : <EllipsisIcon />}
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        {m.is_default ? null : (
                          <DropdownMenuItem
                            onSelect={() =>
                              setDefault.mutate(m.id, {
                                onSuccess: () =>
                                  toast.success(`${m.name} is the ${ENVIRONMENT_LABEL[m.environment]} default`),
                                onError: (e) => toastError(e, "Couldn't change the default model"),
                              })
                            }
                          >
                            <StarIcon />
                            Make default
                          </DropdownMenuItem>
                        )}
                        <DropdownMenuItem onSelect={() => setRenaming(m)}>
                          <PencilIcon />
                          Rename
                        </DropdownMenuItem>
                        <DropdownMenuSeparator />
                        <DropdownMenuItem variant="destructive" onSelect={() => setRemoving(m)}>
                          <Trash2Icon />
                          Remove model
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  ) : null}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
      {models.hasNextPage ? (
        <div className="border-t p-3 text-center">
          <Button variant="ghost" size="sm" onClick={() => models.fetchNextPage()} disabled={models.isFetchingNextPage}>
            {models.isFetchingNextPage ? <Spinner /> : null}
            Show more
          </Button>
        </div>
      ) : null}
      <RenameDialog
        title="Rename model"
        description="The name people see when choosing it. Runs still call the provider's model ID."
        initial={renaming?.name ?? ""}
        open={renaming !== null}
        onOpenChange={(open) => {
          if (!open) setRenaming(null);
        }}
        pending={rename.isPending}
        onSave={(name, setError) => {
          if (!renaming) return;
          rename.mutate(
            { id: renaming.id, name },
            {
              onSuccess: () => {
                setRenaming(null);
                toast.success("Renamed the model");
              },
              onError: (e) => {
                if (isApiError(e) && e.fields?.name) setError(e.fields.name);
                else toastError(e, "Couldn't rename the model");
              },
            },
          );
        }}
      />
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        title={`Remove ${removing?.name ?? "model"}?`}
        description={
          <p>
            Agents can no longer choose it. While it's the default model or an agent uses it, removing is refused. You
            can add it again later.
          </p>
        }
        actionLabel="Remove model"
        pending={remove.isPending}
        onConfirm={() => {
          if (!removing) return;
          const model = removing;
          remove.mutate(model.id, {
            onSuccess: () => {
              setRemoving(null);
              toast.success(`Removed ${model.name}`);
            },
            onError: (e) => {
              setRemoving(null);
              toastError(e, `Couldn't remove ${model.name}`);
            },
          });
        }}
      />
    </Panel>
  );
}
