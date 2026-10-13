import type { Environment, Model, ModelId, OrganizationId } from "@zupflow/sdk";
import { CheckIcon, ChevronsUpDownIcon } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Skeleton } from "@/components/ui/skeleton";
import { useModel, useModelPage } from "@/features/queries";
import { useDebounced } from "@/lib/use-debounced";
import { cn } from "@/lib/utils";
import { routeName } from "./labels";
import { groupByConnection } from "./model-setup";

const NONE = "__none";
const PAGE = 20;

/**
 * Pick one of an environment's models, or none. The server does the
 * searching as people type; the first matches show, grouped by the
 * connection they're reached through, each with its route.
 */
export function ModelPicker({
  orgId,
  environment,
  value,
  onChange,
  none,
  selected,
  disabled,
  label,
  className,
}: {
  orgId: OrganizationId;
  environment: Environment;
  value: ModelId | null;
  onChange: (modelId: ModelId | null) => void;
  /** The first option, meaning no model: "Organization default (…)", "No default". */
  none: React.ReactNode;
  /** The chosen model when the caller already has it, so it shows without a lookup. */
  selected?: Model | null;
  disabled?: boolean;
  label: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim());
  const results = useModelPage(orgId, { environment, q: q || undefined, limit: PAGE }, open);
  // The chosen model shows even when it's past the first matches.
  const known = selected?.id === value ? selected : undefined;
  const lookup = useModel(orgId, value && !known ? value : null);
  const chosen = known ?? lookup.data;
  const items = results.data?.items ?? [];
  const total = results.data?.pagination.total ?? null;
  const choose = (next: ModelId | null) => {
    onChange(next);
    setOpen(false);
  };

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
    >
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="outline"
          role="combobox"
          aria-expanded={open}
          aria-label={label}
          disabled={disabled}
          className={cn("w-full min-w-0 justify-between bg-card font-normal", className)}
        >
          <span className="min-w-0 truncate">
            {!value ? (
              none
            ) : chosen ? (
              <>
                {chosen.name} <span className="text-xs text-muted-foreground">{routeName(chosen.route.kind)}</span>
              </>
            ) : lookup.isError ? (
              <span className="text-muted-foreground">Model not found</span>
            ) : (
              <Skeleton className="inline-block h-3.5 w-28 align-middle" />
            )}
          </span>
          <ChevronsUpDownIcon className="text-muted-foreground" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[--radix-popover-trigger-width] min-w-72 p-0" align="start">
        <Command shouldFilter={false}>
          <CommandInput placeholder="Search name, model ID or author" value={query} onValueChange={setQuery} />
          <CommandList>
            {results.isPending ? (
              <div className="space-y-2 p-2">
                {[0, 1, 2].map((i) => (
                  <Skeleton key={i} className="h-7 w-full" />
                ))}
              </div>
            ) : results.isError ? (
              <p className="px-3 py-6 text-center text-sm text-muted-foreground">
                Couldn't load models. Close this and try again.
              </p>
            ) : (
              <>
                <CommandEmpty>No model matches.</CommandEmpty>
                {!q ? (
                  <CommandGroup>
                    <CommandItem value={NONE} onSelect={() => choose(null)}>
                      <span className="min-w-0 flex-1 truncate">{none}</span>
                      {!value ? <CheckIcon className="size-4" /> : null}
                    </CommandItem>
                  </CommandGroup>
                ) : null}
                {groupByConnection(items).map((group) => (
                  <CommandGroup key={group.models[0]?.route.connection_id} heading={group.label}>
                    {group.models.map((m) => (
                      <CommandItem key={m.id} value={m.id} onSelect={() => choose(m.id)}>
                        <span className="min-w-0 flex-1 truncate">
                          {m.name} <span className="text-xs text-muted-foreground">{routeName(m.route.kind)}</span>
                        </span>
                        {value === m.id ? <CheckIcon className="size-4" /> : null}
                      </CommandItem>
                    ))}
                  </CommandGroup>
                ))}
                {total !== null && total > items.length ? (
                  <p className="border-t px-3 py-2 text-xs text-muted-foreground">
                    Showing {items.length} of {total.toLocaleString("en")}. Type to narrow it down.
                  </p>
                ) : null}
              </>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
