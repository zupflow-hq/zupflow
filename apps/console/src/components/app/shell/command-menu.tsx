import { useNavigate } from "@tanstack/react-router";
import { ActivityIcon, BlocksIcon, BoxesIcon, KeyRoundIcon, PlugIcon, SearchIcon, WebhookIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { Skeleton } from "@/components/ui/skeleton";
import { useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useWorkspacePage } from "@/features/queries";
import { useDebounced } from "@/lib/use-debounced";
import { ENVIRONMENT_LABEL } from "../environment";

const PAGES = [
  { label: "Workspaces", to: "/$org/workspaces", icon: BoxesIcon },
  { label: "Runs", to: "/$org/runs", icon: ActivityIcon },
  { label: "App catalog", to: "/$org/apps", icon: BlocksIcon },
  { label: "Connect", to: "/$org/connect", icon: PlugIcon },
  { label: "API keys", to: "/$org/api-keys", icon: KeyRoundIcon },
  { label: "Webhooks", to: "/$org/webhooks", icon: WebhookIcon },
] as const;

/**
 * ⌘K: jump to any workspace by name, slug or external ID, or to a page.
 * Workspaces are searched on the server; there can be thousands.
 */
export function CommandMenu() {
  const org = useOrg();
  const environment = useEnvironment();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim(), 200);
  const workspaces = useWorkspacePage(org.id, { environment, q: q || undefined, limit: 8 }, open);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const close = () => {
    setOpen(false);
    setQuery("");
  };
  const pages = PAGES.filter((p) => !query || p.label.toLowerCase().includes(query.trim().toLowerCase()));
  const items = workspaces.data?.items ?? [];

  return (
    <>
      <Button
        variant="outline"
        onClick={() => setOpen(true)}
        className="h-8 gap-2 bg-background px-2.5 font-normal text-muted-foreground shadow-none max-sm:size-8 max-sm:p-0 sm:w-64 sm:justify-start"
        aria-label="Search"
      >
        <SearchIcon />
        <span className="hidden flex-1 text-left sm:inline">Search workspaces…</span>
        <KbdGroup className="hidden sm:inline-flex">
          <Kbd>⌘</Kbd>
          <Kbd>K</Kbd>
        </KbdGroup>
      </Button>
      <Dialog open={open} onOpenChange={(o) => (o ? setOpen(true) : close())}>
        <DialogContent className="top-[20%] translate-y-0 overflow-hidden p-0 sm:max-w-xl" showCloseButton={false}>
          <DialogTitle className="sr-only">Search</DialogTitle>
          <DialogDescription className="sr-only">Find a workspace or go to a page.</DialogDescription>
          <Command shouldFilter={false} className="[&_[cmdk-group-heading]]:text-muted-foreground">
            <CommandInput
              placeholder={`Search ${ENVIRONMENT_LABEL[environment]} workspaces by name, slug or external ID`}
              value={query}
              onValueChange={setQuery}
              className="h-12"
            />
            <CommandList className="max-h-96">
              {workspaces.isPending && open ? (
                <div className="space-y-2 p-3">
                  {[0, 1, 2].map((i) => (
                    <Skeleton key={i} className="h-8 w-full" />
                  ))}
                </div>
              ) : (
                <>
                  <CommandEmpty>Nothing matches “{query}”.</CommandEmpty>
                  {items.length > 0 ? (
                    <CommandGroup heading={`${ENVIRONMENT_LABEL[environment]} workspaces`}>
                      {items.map((w) => (
                        <CommandItem
                          key={w.id}
                          value={`ws-${w.id}`}
                          onSelect={() => {
                            close();
                            void navigate({ to: "/$org/w/$ws", params: { org: org.slug, ws: w.id } });
                          }}
                        >
                          <BoxesIcon className="text-muted-foreground" />
                          <span className="min-w-0 flex-1 truncate">{w.name}</span>
                          <span className="truncate font-mono text-[11px] text-muted-foreground">
                            {w.external_id ?? w.slug}
                          </span>
                        </CommandItem>
                      ))}
                    </CommandGroup>
                  ) : null}
                  {pages.length > 0 ? (
                    <CommandGroup heading="Pages">
                      {pages.map((p) => (
                        <CommandItem
                          key={p.to}
                          value={`page-${p.to}`}
                          onSelect={() => {
                            close();
                            void navigate({ to: p.to, params: { org: org.slug } });
                          }}
                        >
                          <p.icon className="text-muted-foreground" />
                          {p.label}
                        </CommandItem>
                      ))}
                    </CommandGroup>
                  ) : null}
                </>
              )}
            </CommandList>
          </Command>
        </DialogContent>
      </Dialog>
    </>
  );
}
