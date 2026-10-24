import { Outlet } from "@tanstack/react-router";
import { EnvBadge } from "@/components/app/environment";
import { LinkTabs } from "@/components/app/link-tabs";
import { MonoValue } from "@/components/app/misc";
import { NestedPage, PageContainer } from "@/components/app/page";
import { useCurrentWorkspace, useOrg } from "@/features/context";
import { shortId } from "@/lib/format";

/**
 * A workspace is one of thousands of customer tenants, so it's a page you
 * open from the list, not a context in the sidebar. Its sections are tabs.
 */
export function WorkspaceLayout() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const base = `/${org.slug}/w/${ws.id}`;
  const tabs = [
    { label: "Overview", href: base, exact: true },
    { label: "Agents", href: `${base}/agents` },
    { label: "Installations", href: `${base}/installations` },
    { label: "Channels", href: `${base}/channels` },
    { label: "Runs", href: `${base}/runs` },
    { label: "Apps", href: `${base}/apps` },
    { label: "Connect sessions", href: `${base}/connect-sessions` },
    { label: "Settings", href: `${base}/settings` },
  ];

  return (
    <PageContainer>
      <header className="mb-4 min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h1 className="truncate text-[1.6rem] leading-tight font-semibold tracking-[-0.02em]">{ws.name}</h1>
          <EnvBadge environment={ws.environment} />
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-muted-foreground">
          <span className="inline-flex items-center gap-1.5">
            ID <MonoValue value={ws.id} display={shortId(ws.id)} />
          </span>
          {ws.external_id ? (
            <span className="inline-flex items-center gap-1.5" title="This customer's ID in your platform">
              External ID <MonoValue value={ws.external_id} />
            </span>
          ) : null}
        </div>
      </header>
      <LinkTabs tabs={tabs} label="Workspace sections" />
      <div className="pt-6">
        <NestedPage>
          <Outlet />
        </NestedPage>
      </div>
    </PageContainer>
  );
}
