import type { Environment } from "@zupflow/sdk";
import { useSwitchEnvironment } from "@/features/environment";
import { cn } from "@/lib/utils";

export const ENVIRONMENT_LABEL: Record<Environment, string> = {
  sandbox: "Sandbox",
  production: "Production",
};

/**
 * The environment tag. Its chamfered corners come from the brand mark and are
 * used for nothing else, so the environment is never mistaken for a status.
 */
export function EnvBadge({
  environment,
  size = "default",
  className,
}: {
  environment: Environment;
  size?: "sm" | "default";
  className?: string;
}) {
  return (
    <span
      className={cn(
        "chamfer inline-flex shrink-0 items-center gap-1.5 font-mono font-semibold uppercase tracking-[0.08em]",
        size === "sm" ? "h-[18px] px-1.5 text-[9.5px]" : "h-5 px-2 text-[10.5px]",
        environment === "production" ? "bg-production-soft text-production-ink" : "bg-sandbox-soft text-sandbox-ink",
        className,
      )}
    >
      <span
        aria-hidden
        className={cn("size-1.5 shrink-0", environment === "production" ? "bg-production" : "bg-sandbox")}
      />
      {ENVIRONMENT_LABEL[environment]}
    </span>
  );
}

/**
 * Shown across the top of the Console while it is in Sandbox, the way a
 * dashboard marks test mode. Production has no banner; the switch says where you are.
 */
export function SandboxBanner() {
  const switchTo = useSwitchEnvironment();
  return (
    <div className="relative flex h-8 items-center justify-center gap-2 bg-sandbox-soft px-3 text-xs text-sandbox-ink md:rounded-t-xl">
      <div aria-hidden className="env-rail-sandbox absolute inset-x-0 bottom-0 h-[3px]" />
      <EnvBadge environment="sandbox" size="sm" className="bg-transparent px-0" />
      <span className="hidden truncate sm:inline">Test data only. Nothing here reaches Production.</span>
      <button
        type="button"
        onClick={() => switchTo("production")}
        className="ml-1 shrink-0 font-medium underline-offset-4 outline-none hover:underline focus-visible:underline"
      >
        Switch to Production
      </button>
    </div>
  );
}
