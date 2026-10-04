import { Link, useLocation } from "@tanstack/react-router";
import { cn } from "@/lib/utils";

export interface LinkTab {
  label: React.ReactNode;
  /** Resolved path, used both to navigate and to decide which tab is active. */
  href: string;
  exact?: boolean;
}

/**
 * Route-backed underline tabs. Same look as `LineTabsList`: the active bar
 * overlaps the list's 1px bottom border exactly.
 */
export function LinkTabs({ tabs, label, className }: { tabs: LinkTab[]; label: string; className?: string }) {
  const pathname = useLocation({ select: (l) => l.pathname.replace(/\/$/, "") });
  return (
    <div className={cn("-mx-4 overflow-x-auto px-4 sm:mx-0 sm:px-0", className)}>
      <nav aria-label={label} className="flex min-w-max gap-5 border-b">
        {tabs.map((tab) => {
          const active = tab.exact
            ? pathname === tab.href
            : pathname === tab.href || pathname.startsWith(`${tab.href}/`);
          return (
            <Link
              key={tab.href}
              to={tab.href}
              aria-current={active ? "page" : undefined}
              className={cn(
                "relative inline-flex h-10 items-center gap-1.5 px-0.5 text-sm font-medium whitespace-nowrap text-muted-foreground transition-colors outline-none hover:text-foreground focus-visible:text-foreground",
                "after:absolute after:inset-x-0 after:-bottom-px after:h-0.5 after:bg-brand-ink after:opacity-0 after:transition-opacity dark:after:bg-brand",
                active && "text-foreground after:opacity-100",
              )}
            >
              {tab.label}
            </Link>
          );
        })}
      </nav>
    </div>
  );
}
