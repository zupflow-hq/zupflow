import type { Environment } from "@zupflow/sdk";
import { CheckIcon, ChevronsUpDownIcon } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { SidebarMenuButton, useSidebar } from "@/components/ui/sidebar";
import { useEnvironment, useSwitchEnvironment } from "@/features/environment";
import { cn } from "@/lib/utils";
import { ENVIRONMENT_LABEL } from "../environment";

const DESCRIPTION: Record<Environment, string> = {
  production: "Live customers, live keys",
  sandbox: "Test workspaces, test keys",
};

function Swatch({ environment, className }: { environment: Environment; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn(
        "chamfer size-3 shrink-0",
        environment === "production" ? "bg-production" : "env-rail-sandbox",
        className,
      )}
    />
  );
}

/** Chooses the environment the whole Console shows, beneath the organization it belongs to. */
export function EnvironmentMenu() {
  const environment = useEnvironment();
  const switchTo = useSwitchEnvironment();
  const { isMobile, setOpenMobile } = useSidebar();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <SidebarMenuButton
          tooltip={`Environment: ${ENVIRONMENT_LABEL[environment]}`}
          className="h-9 border bg-card shadow-xs hover:bg-card data-[state=open]:ring-2 data-[state=open]:ring-ring/30 group-data-[collapsible=icon]:border-transparent group-data-[collapsible=icon]:bg-transparent group-data-[collapsible=icon]:shadow-none"
        >
          <Swatch environment={environment} className="ml-0.5" />
          <span className="flex-1 truncate font-medium">{ENVIRONMENT_LABEL[environment]}</span>
          <ChevronsUpDownIcon className="size-3.5 text-muted-foreground" />
        </SidebarMenuButton>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-60" align="start" sideOffset={6}>
        <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">Environment</DropdownMenuLabel>
        {(["production", "sandbox"] as const).map((env) => (
          <DropdownMenuItem
            key={env}
            onSelect={() => {
              if (isMobile) setOpenMobile(false);
              if (env !== environment) switchTo(env);
            }}
            className="items-start py-2"
          >
            <Swatch environment={env} className="mt-1" />
            <span className="flex-1">
              <span className="block font-medium">{ENVIRONMENT_LABEL[env]}</span>
              <span className="block text-xs text-muted-foreground">{DESCRIPTION[env]}</span>
            </span>
            {env === environment ? <CheckIcon className="mt-0.5 size-4" /> : null}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
