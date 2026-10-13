import { Link, useLocation, useNavigate } from "@tanstack/react-router";
import {
  ActivityIcon,
  BlocksIcon,
  BoxesIcon,
  CheckIcon,
  ChevronsUpDownIcon,
  KeyRoundIcon,
  LayoutDashboardIcon,
  type LucideIcon,
  MailIcon,
  PlugIcon,
  PlusIcon,
  ScrollTextIcon,
  SettingsIcon,
  SparklesIcon,
  UsersIcon,
  WebhookIcon,
} from "lucide-react";
import { useState } from "react";
import { ZupflowMark } from "@/components/brand/logo";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
  useSidebar,
} from "@/components/ui/sidebar";
import { useOrg } from "@/features/context";
import { InvitationsDialog, usePendingInvitations } from "@/features/invitations/invitation-list";
import { useViewer } from "@/features/queries";
import { EnvironmentMenu } from "./environment-menu";
import { UserMenu } from "./user-menu";

interface NavItem {
  label: string;
  icon: LucideIcon;
  to: string;
  params: Record<string, string>;
  exact?: boolean;
  /** Other path prefixes that also mark this item active. */
  also?: string[];
}

function hrefFor(item: NavItem) {
  return item.to.replace(/\$(\w+)/g, (_, key: string) => encodeURIComponent(item.params[key] ?? ""));
}

function NavLinks({ items }: { items: NavItem[] }) {
  const pathname = useLocation({ select: (l) => l.pathname.replace(/\/$/, "") });
  const { isMobile, setOpenMobile } = useSidebar();
  return (
    <SidebarMenu>
      {items.map((item) => {
        const href = hrefFor(item);
        const prefixes = [href, ...(item.also ?? []).map((to) => hrefFor({ ...item, to }))];
        const active = item.exact
          ? pathname === href
          : prefixes.some((p) => pathname === p || pathname.startsWith(`${p}/`));
        return (
          <SidebarMenuItem key={item.to}>
            <SidebarMenuButton
              asChild
              isActive={active}
              tooltip={item.label}
              className="relative data-[active=true]:font-medium data-[active=true]:before:absolute data-[active=true]:before:inset-y-2 data-[active=true]:before:left-0 data-[active=true]:before:w-[2.5px] data-[active=true]:before:rounded-r-full data-[active=true]:before:bg-brand"
            >
              <Link
                to={item.to}
                params={item.params}
                onClick={() => {
                  if (isMobile) setOpenMobile(false);
                }}
              >
                <item.icon />
                <span>{item.label}</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        );
      })}
    </SidebarMenu>
  );
}

function OrgSwitcher() {
  const org = useOrg();
  const viewer = useViewer();
  const navigate = useNavigate();
  const pending = usePendingInvitations().length;
  const [invitationsOpen, setInvitationsOpen] = useState(false);
  const { setOpenMobile } = useSidebar();
  return (
    <>
      <InvitationsDialog
        open={invitationsOpen}
        onOpenChange={setInvitationsOpen}
        onJoined={() => setOpenMobile(false)}
      />
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <SidebarMenuButton size="lg" className="data-[state=open]:bg-sidebar-accent">
            <span className="grid size-8 shrink-0 place-items-center rounded-md bg-[#10191D] dark:bg-[#132027]">
              <ZupflowMark className="size-5" />
            </span>
            <span className="grid min-w-0 flex-1 text-left leading-tight">
              <span className="truncate font-semibold">{org.name}</span>
              <span className="truncate text-xs text-muted-foreground capitalize">{org.role}</span>
            </span>
            <ChevronsUpDownIcon className="ml-auto size-4 text-muted-foreground" />
          </SidebarMenuButton>
        </DropdownMenuTrigger>
        <DropdownMenuContent className="w-64" align="start" sideOffset={6}>
          <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">Organizations</DropdownMenuLabel>
          {viewer.data?.organizations.map((o) => (
            <DropdownMenuItem key={o.id} onSelect={() => navigate({ to: "/$org", params: { org: o.slug } })}>
              <span className="grid size-6 place-items-center rounded bg-muted font-mono text-[11px] font-semibold">
                {o.name.slice(0, 1).toUpperCase()}
              </span>
              <span className="flex-1 truncate">{o.name}</span>
              {o.id === org.id ? <CheckIcon className="size-4" /> : null}
            </DropdownMenuItem>
          ))}
          <DropdownMenuSeparator />
          {pending > 0 ? (
            <DropdownMenuItem onSelect={() => setInvitationsOpen(true)}>
              <MailIcon />
              {pending === 1 ? "1 invitation" : `${pending} invitations`}
            </DropdownMenuItem>
          ) : null}
          <DropdownMenuItem onSelect={() => navigate({ to: "/onboarding" })}>
            <PlusIcon />
            Create organization
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </>
  );
}

export function AppSidebar() {
  const org = useOrg();
  const o = { org: org.slug };

  const orgItems: NavItem[] = [
    { label: "Overview", icon: LayoutDashboardIcon, to: "/$org", params: o, exact: true },
    { label: "Workspaces", icon: BoxesIcon, to: "/$org/workspaces", params: o, also: ["/$org/w"] },
    { label: "Runs", icon: ActivityIcon, to: "/$org/runs", params: o },
    { label: "App catalog", icon: BlocksIcon, to: "/$org/apps", params: o },
  ];
  const developerItems: NavItem[] = [
    { label: "Connect", icon: PlugIcon, to: "/$org/connect", params: o },
    { label: "Model providers", icon: SparklesIcon, to: "/$org/model-providers", params: o },
    { label: "API keys", icon: KeyRoundIcon, to: "/$org/api-keys", params: o },
    { label: "Webhooks", icon: WebhookIcon, to: "/$org/webhooks", params: o },
  ];
  const orgAdminItems: NavItem[] = [
    { label: "Members", icon: UsersIcon, to: "/$org/members", params: o },
    { label: "Audit log", icon: ScrollTextIcon, to: "/$org/audit-log", params: o },
    { label: "Settings", icon: SettingsIcon, to: "/$org/settings", params: o },
  ];

  return (
    <Sidebar collapsible="icon" variant="inset">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <OrgSwitcher />
          </SidebarMenuItem>
          <SidebarMenuItem>
            <EnvironmentMenu />
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <NavLinks items={orgItems} />
          </SidebarGroupContent>
        </SidebarGroup>
        <SidebarGroup>
          <SidebarGroupLabel>Developers</SidebarGroupLabel>
          <SidebarGroupContent>
            <NavLinks items={developerItems} />
          </SidebarGroupContent>
        </SidebarGroup>
        <SidebarGroup>
          <SidebarGroupLabel>Organization</SidebarGroupLabel>
          <SidebarGroupContent>
            <NavLinks items={orgAdminItems} />
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <UserMenu />
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
