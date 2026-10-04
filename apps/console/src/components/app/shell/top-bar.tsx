import { Link, useMatches } from "@tanstack/react-router";
import { Fragment } from "react";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Separator } from "@/components/ui/separator";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { useOptionalWorkspaceId } from "@/features/context";
import { useWorkspace } from "@/features/queries";
import { useCrumbs } from "@/lib/crumbs";
import { CommandMenu } from "./command-menu";

/**
 * Breadcrumbs come from each route's `staticData.crumb`, plus a label the
 * current page may set for its resource (an agent's name, say).
 */
export function TopBar() {
  const matches = useMatches();
  const detail = useCrumbs();
  const ws = useOptionalWorkspaceId();
  const workspace = useWorkspace(ws);
  const crumbs = matches.flatMap((m) => {
    const crumb = m.staticData.crumb;
    if (!crumb) return [];
    if (crumb === "$workspace") {
      const list = { label: "Workspaces", to: m.pathname.replace(/\/w\/[^/]+$/, "/workspaces") };
      return workspace.data ? [list, { label: workspace.data.name, to: m.pathname }] : [list];
    }
    return [{ label: crumb, to: m.pathname }];
  });
  if (detail) crumbs.push({ label: detail, to: "" });

  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b bg-card px-3 md:rounded-t-xl">
      <SidebarTrigger className="-ml-0.5" />
      <Separator orientation="vertical" className="mr-1 data-[orientation=vertical]:h-4" />
      <Breadcrumb className="min-w-0 flex-1">
        <BreadcrumbList className="flex-nowrap">
          {crumbs.map((c, i) => {
            const last = i === crumbs.length - 1;
            return (
              <Fragment key={`${c.to}|${c.label}`}>
                {i > 0 ? <BreadcrumbSeparator className="hidden sm:block" /> : null}
                <BreadcrumbItem className={last ? "min-w-0" : "hidden sm:inline-flex"}>
                  {last || !c.to ? (
                    <BreadcrumbPage className="truncate">{c.label}</BreadcrumbPage>
                  ) : (
                    <BreadcrumbLink asChild>
                      <Link to={c.to}>{c.label}</Link>
                    </BreadcrumbLink>
                  )}
                </BreadcrumbItem>
              </Fragment>
            );
          })}
        </BreadcrumbList>
      </Breadcrumb>
      <CommandMenu />
    </header>
  );
}
