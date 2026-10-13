import type { ModelConnection, Organization, ProviderModel } from "@zupflow/sdk";
import { CheckIcon, PlusIcon, SearchIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { QueryError } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty";
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useCreateModel, useProviderModels } from "@/features/queries";
import { toastError } from "@/lib/errors";
import { useDebounced } from "@/lib/use-debounced";
import { contextWindowLabel, pricingLabel } from "./labels";

/**
 * Search the provider's live list for this connection's key (on the server,
 * as you type) and add models from it. Models that can't call tools are shown
 * but can't be added: an agent's work is calling tools.
 */
export function AddModelsDialog({
  org,
  connection,
  open,
  onOpenChange,
}: {
  org: Organization;
  connection: ModelConnection;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim(), 250);
  useEffect(() => {
    if (!open) setQuery("");
  }, [open]);
  const results = useProviderModels(org.id, connection.id, q, open);
  const items = results.data?.pages.flatMap((p) => p.items) ?? [];
  const searching = results.isFetching && !results.isFetchingNextPage;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[min(40rem,calc(100dvh-2rem))] flex-col gap-0 p-0 sm:max-w-2xl">
        <DialogHeader className="border-b p-4 sm:p-5">
          <DialogTitle>Add models from {connection.label}</DialogTitle>
          <DialogDescription>
            What the provider lists for this key. Added models can be chosen as the default or by an agent.
          </DialogDescription>
          <InputGroup className="mt-3">
            <InputGroupAddon>{searching ? <Spinner /> : <SearchIcon />}</InputGroupAddon>
            <InputGroupInput
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search by name, ID or author"
              aria-label="Search models"
            />
          </InputGroup>
        </DialogHeader>
        <div className="min-h-0 flex-1 overflow-y-auto">
          {results.isError ? (
            <QueryError
              error={results.error}
              onRetry={() => results.refetch()}
              what="the provider's models"
              className="m-4"
            />
          ) : results.isPending ? (
            <ProviderModelsSkeleton />
          ) : items.length === 0 ? (
            <Empty className="py-10">
              <EmptyHeader>
                <EmptyTitle>No models match “{q}”</EmptyTitle>
                <EmptyDescription>Try a shorter search, an author such as “google”, or a model ID.</EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <ul className="divide-y">
              {items.map((m) => (
                <ProviderModelRow key={m.provider_model_id} org={org} connection={connection} model={m} />
              ))}
            </ul>
          )}
          {results.hasNextPage ? (
            <div className="border-t p-3 text-center">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => results.fetchNextPage()}
                disabled={results.isFetchingNextPage}
              >
                {results.isFetchingNextPage ? <Spinner /> : null}
                Show more
              </Button>
            </div>
          ) : null}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function ProviderModelRow({
  org,
  connection,
  model,
}: {
  org: Organization;
  connection: ModelConnection;
  model: ProviderModel;
}) {
  const create = useCreateModel(org.id);
  const added = Boolean(model.added_model_id) || create.isSuccess;
  const facts = [model.author, contextWindowLabel(model.context_window)].filter(Boolean).join(" · ");
  return (
    <li className="flex items-center gap-3 px-4 py-3 sm:px-5">
      <div className="min-w-0 flex-1 space-y-0.5">
        <p className="truncate text-sm font-medium">{model.name}</p>
        <p className="truncate font-mono text-xs text-muted-foreground">{model.provider_model_id}</p>
        <p className="truncate text-xs text-muted-foreground">
          {facts}
          {model.pricing ? <span className="hidden sm:inline"> · {pricingLabel(model.pricing)}</span> : null}
        </p>
      </div>
      {added ? (
        <span className="flex shrink-0 items-center gap-1 text-xs font-medium text-success">
          <CheckIcon className="size-3.5" />
          Added
        </span>
      ) : !model.supports_tools ? (
        <span className="shrink-0 text-right text-xs text-muted-foreground">Can't call tools</span>
      ) : (
        <Button
          variant="outline"
          size="sm"
          className="shrink-0"
          disabled={create.isPending}
          onClick={() =>
            create.mutate(
              { connection_id: connection.id, provider_model_id: model.provider_model_id },
              {
                onSuccess: (m) => toast.success(`Added ${m.name}`),
                onError: (e) => toastError(e, `Couldn't add ${model.name}`),
              },
            )
          }
        >
          {create.isPending ? <Spinner /> : <PlusIcon />}
          Add
        </Button>
      )}
    </li>
  );
}

function ProviderModelsSkeleton() {
  return (
    <div className="divide-y" role="status" aria-busy aria-label="Loading">
      {[46, 38, 52, 34, 44].map((w) => (
        <div key={w} className="flex items-center gap-3 px-4 py-3 sm:px-5">
          <div className="flex-1 space-y-1.5">
            <Skeleton className="h-3.5" style={{ width: `${w}%` }} />
            <Skeleton className="h-3 w-1/3" />
            <Skeleton className="h-3 w-1/2" />
          </div>
          <Skeleton className="h-8 w-16" />
        </div>
      ))}
    </div>
  );
}
