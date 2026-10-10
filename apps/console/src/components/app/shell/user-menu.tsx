import { useNavigate } from "@tanstack/react-router";
import {
  ChevronsUpDownIcon,
  DatabaseZapIcon,
  LogOutIcon,
  MailIcon,
  MonitorIcon,
  MoonIcon,
  SunIcon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { SidebarMenu, SidebarMenuButton, SidebarMenuItem, useSidebar } from "@/components/ui/sidebar";
import { session } from "@/features/auth/session";
import { useViewerUser } from "@/features/context";
import { InvitationsDialog, usePendingInvitations } from "@/features/invitations/invitation-list";
import { type ThemePreference, useTheme } from "@/lib/theme";

export function initials(name: string) {
  return (
    name
      .split(/\s+/)
      .filter(Boolean)
      .slice(0, 2)
      .map((p) => p[0]?.toUpperCase())
      .join("") || "?"
  );
}

export function UserMenu() {
  const user = useViewerUser();
  const { preference, setPreference } = useTheme();
  const navigate = useNavigate();
  const controls = session.controls();
  const pending = usePendingInvitations().length;
  const [invitationsOpen, setInvitationsOpen] = useState(false);
  const { setOpenMobile } = useSidebar();

  return (
    <SidebarMenu>
      <InvitationsDialog
        open={invitationsOpen}
        onOpenChange={setInvitationsOpen}
        onJoined={() => setOpenMobile(false)}
      />
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg" className="data-[state=open]:bg-sidebar-accent">
              <Avatar className="size-8 rounded-md">
                <AvatarFallback className="rounded-md bg-secondary text-xs font-semibold">
                  {initials(user.name)}
                </AvatarFallback>
              </Avatar>
              <span className="grid min-w-0 flex-1 text-left leading-tight">
                <span className="truncate font-medium">{user.name}</span>
                <span className="truncate text-xs text-muted-foreground">{user.email}</span>
              </span>
              {pending > 0 ? (
                <span className="size-2 shrink-0 rounded-full bg-brand" role="img" aria-label="Pending invitations" />
              ) : null}
              <ChevronsUpDownIcon className="ml-auto size-4 text-muted-foreground" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent className="w-60" side="top" align="start" sideOffset={6}>
            <DropdownMenuItem onSelect={() => setInvitationsOpen(true)}>
              <MailIcon />
              Invitations
              {pending > 0 ? (
                <span className="ml-auto rounded-full bg-brand-soft px-1.5 font-mono text-[11px] text-brand-ink">
                  {pending}
                </span>
              ) : null}
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">Theme</DropdownMenuLabel>
            <DropdownMenuRadioGroup value={preference} onValueChange={(v) => setPreference(v as ThemePreference)}>
              <DropdownMenuRadioItem value="light">
                <SunIcon /> Light
              </DropdownMenuRadioItem>
              <DropdownMenuRadioItem value="dark">
                <MoonIcon /> Dark
              </DropdownMenuRadioItem>
              <DropdownMenuRadioItem value="system">
                <MonitorIcon /> System
              </DropdownMenuRadioItem>
            </DropdownMenuRadioGroup>
            <DropdownMenuSeparator />
            {controls.mode === "mock" ? (
              <>
                <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
                  Mock API · data stays in this browser
                </DropdownMenuLabel>
                <DropdownMenuItem
                  onSelect={async () => {
                    controls.resetDemoData();
                    await navigate({ to: "/" });
                    toast.success("Demo data reset");
                  }}
                >
                  <DatabaseZapIcon />
                  Reset demo data
                </DropdownMenuItem>
              </>
            ) : null}
            {controls.mode === "local" ? (
              <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
                Local mode · no sign-in on this server
              </DropdownMenuLabel>
            ) : (
              <DropdownMenuItem
                onSelect={() => {
                  session.signOut();
                  void navigate({ to: "/sign-in" });
                }}
              >
                <LogOutIcon />
                Sign out
              </DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
