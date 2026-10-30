import type { MemoryType, MemoryTypeId, OrganizationId } from "@zupflow/sdk";
import { CheckIcon, ChevronsUpDownIcon } from "lucide-react";
import { useState } from "react";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useMemoryKinds, useMemoryType, useMemoryTypeList } from "@/features/queries";
import { cn } from "@/lib/utils";
import { kindName } from "./labels";

/** A choice offered around the organization's types: "Organization default", "Off", "No default". */
export interface MemoryChoice {
  value: string;
  label: React.ReactNode;
  /** What a search matches it by. */
  search: string;
}

const PAGE = 100;

/** Text as a search compares it: lower case, without accents. */
const folded = (text: string) => text.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();
const wordsOf = (text: string) =>
  folded(text)
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean);

/**
 * Finds a choice or a type by how its words start: each word typed begins one
 * of its words. "off" finds Off alone, where a fuzzy match would also find
 * "Organization default" by its letters and could put it first.
 */
function byWordStart(_value: string, search: string, keywords: string[] = []): number {
  const words = wordsOf(keywords.join(" "));
  return wordsOf(search).every((typed) => words.some((word) => word.startsWith(typed))) ? 1 : 0;
}

/**
 * Pick one of the organization's memory types, or a choice around them. The
 * server lists types by name a page at a time, with no search: the box
 * narrows the pages read so far, and "Show more types" reads the next one.
 */
export function MemoryTypePicker({
  orgId,
  value,
  onChange,
  before = [],
  after = [],
  selected,
  disabled,
  label,
  className,
}: {
  orgId: OrganizationId;
  /** A memory type's ID, or a choice's value. */
  value: string;
  onChange: (value: string) => void;
  before?: MemoryChoice[];
  after?: MemoryChoice[];
  /** The chosen type when the caller already has it, so it shows without a lookup. */
  selected?: MemoryType | null;
  disabled?: boolean;
  label: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const choices = [...before, ...after];
  const choice = choices.find((c) => c.value === value);
  const types = useMemoryTypeList(orgId, PAGE, open);
  const kinds = useMemoryKinds(orgId);
  const items = types.data?.pages.flatMap((page) => page.items) ?? [];
  // The chosen type as the caller or the pages read have it, else looked up: it shows even past those pages.
  const known = choice ? undefined : selected?.id === value ? selected : items.find((t) => t.id === value);
  const lookup = useMemoryType(orgId, !choice && !known ? (value as MemoryTypeId) : null);
  const chosen = known ?? lookup.data;
  const total = types.data?.pages[0]?.pagination.total ?? null;
  const choose = (next: string) => {
    onChange(next);
    setOpen(false);
  };
  const choiceItem = (c: MemoryChoice) => (
    <CommandItem key={c.value} value={c.value} keywords={[c.search]} onSelect={() => choose(c.value)}>
      <span className="min-w-0 flex-1 truncate">{c.label}</span>
      {value === c.value ? <CheckIcon className="size-4" /> : null}
    </CommandItem>
  );

  return (
    <Popover open={open} onOpenChange={setOpen}>
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
            {choice ? (
              choice.label
            ) : chosen ? (
              <>
                {chosen.name}{" "}
                <span className="text-xs text-muted-foreground">
                  {chosen.served ? kindNote(chosen, kindName(kinds.data, chosen.kind)) : "not served"}
                </span>
              </>
            ) : lookup.isError ? (
              <span className="text-muted-foreground">Memory type not found</span>
            ) : (
              <Skeleton className="inline-block h-3.5 w-28 align-middle" />
            )}
          </span>
          <ChevronsUpDownIcon className="text-muted-foreground" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className="w-(--radix-popover-trigger-width) max-w-(--radix-popover-content-available-width) min-w-72 p-0"
        align="start"
      >
        <Command filter={byWordStart}>
          <CommandInput placeholder="Search memory types" />
          <CommandList>
            <CommandEmpty>
              No memory type matches.{types.hasNextPage ? " Show more types to look further." : ""}
            </CommandEmpty>
            {before.length > 0 ? <CommandGroup>{before.map(choiceItem)}</CommandGroup> : null}
            {types.isPending ? (
              <div className="space-y-2 p-2" role="status" aria-label="Loading memory types">
                {[0, 1].map((i) => (
                  <Skeleton key={i} className="h-7 w-full" />
                ))}
              </div>
            ) : types.isError ? (
              <p className="px-3 py-4 text-center text-sm text-muted-foreground">
                Couldn't load memory types. Close this and try again.
              </p>
            ) : items.length > 0 ? (
              <CommandGroup heading="Memory types">
                {items.map((t) => (
                  <CommandItem
                    key={t.id}
                    value={t.id}
                    keywords={[t.name, kindName(kinds.data, t.kind)]}
                    onSelect={() => choose(t.id)}
                  >
                    <span className="min-w-0 flex-1 truncate" title={t.name}>
                      {t.name}{" "}
                      <span className="text-xs text-muted-foreground">
                        {t.served ? kindNote(t, kindName(kinds.data, t.kind)) : t.kind}
                      </span>
                    </span>
                    {t.served ? null : <StatusPill tone="warning">Not served</StatusPill>}
                    {value === t.id ? <CheckIcon className="size-4" /> : null}
                  </CommandItem>
                ))}
                {types.hasNextPage ? (
                  <CommandItem
                    forceMount
                    value="__more"
                    disabled={types.isFetchingNextPage}
                    onSelect={() => void types.fetchNextPage()}
                    className="text-muted-foreground"
                  >
                    {types.isFetchingNextPage ? <Spinner /> : null}
                    Show more types
                    {total !== null ? <span className="ml-auto text-xs">{total - items.length} more</span> : null}
                  </CommandItem>
                ) : null}
              </CommandGroup>
            ) : null}
            {after.length > 0 ? (
              <>
                <CommandSeparator />
                <CommandGroup>{after.map(choiceItem)}</CommandGroup>
              </>
            ) : null}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

/** A type's kind beside its name, unless the name already says it ("Notes", of notes). */
function kindNote(type: Pick<MemoryType, "name">, kind: string): string {
  return type.name.trim().toLowerCase() === kind.trim().toLowerCase() ? "" : kind;
}
