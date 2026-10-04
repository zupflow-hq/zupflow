import type { Environment } from "@zupflow/sdk";
import { CheckIcon, ChevronsUpDownIcon } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Skeleton } from "@/components/ui/skeleton";
import { useWorkspace, useWorkspacePage } from "@/features/queries";
import { useDebounced } from "@/lib/use-debounced";
import { cn } from "@/lib/utils";

const ALL = "__all";

/**
 * Pick one workspace out of thousands. The server does the searching; this
 * shows the first matches and asks for a narrower query beyond that.
 */
export function WorkspacePicker({
  orgId,
  environment,
  value,
  onChange,
  allLabel = "All workspaces",
  className,
}: {
  orgId: string;
  environment?: Environment;
  value: string | undefined;
  onChange: (workspaceId: string | undefined) => void;
  allLabel?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim());
  const results = useWorkspacePage(orgId, { environment, q: q || undefined, limit: 20 }, open);
  const selected = useWorkspace(value ?? "");
  const items = results.data?.items ?? [];
  const total = results.data?.pagination.total ?? 0;

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
          variant="outline"
          role="combobox"
          aria-expanded={open}
          aria-label="Workspace"
          className={cn("w-full justify-between bg-card font-normal sm:w-64", className)}
        >
          <span className="truncate">{value ? (selected.data?.name ?? "Loading…") : allLabel}</span>
          <ChevronsUpDownIcon className="text-muted-foreground" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[--radix-popover-trigger-width] min-w-72 p-0" align="start">
        <Command shouldFilter={false}>
          <CommandInput placeholder="Search name, slug or external ID" value={query} onValueChange={setQuery} />
          <CommandList>
            {results.isPending ? (
              <div className="space-y-2 p-2">
                {[0, 1, 2].map((i) => (
                  <Skeleton key={i} className="h-7 w-full" />
                ))}
              </div>
            ) : (
              <>
                <CommandEmpty>No workspace matches.</CommandEmpty>
                <CommandGroup>
                  {!q ? (
                    <CommandItem
                      value={ALL}
                      onSelect={() => {
                        onChange(undefined);
                        setOpen(false);
                      }}
                    >
                      <span className="flex-1">{allLabel}</span>
                      {!value ? <CheckIcon className="size-4" /> : null}
                    </CommandItem>
                  ) : null}
                  {items.map((w) => (
                    <CommandItem
                      key={w.id}
                      value={w.id}
                      onSelect={() => {
                        onChange(w.id);
                        setOpen(false);
                      }}
                    >
                      <span className="min-w-0 flex-1 truncate">{w.name}</span>
                      <span className="truncate font-mono text-[11px] text-muted-foreground">{w.slug}</span>
                      {value === w.id ? <CheckIcon className="size-4" /> : null}
                    </CommandItem>
                  ))}
                </CommandGroup>
                {total > items.length ? (
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
