import type {
  AgentStatus,
  ConfigurationStatus,
  ConnectSessionStatus,
  InstallationStatus,
  RunStatus,
  ToolEffect,
} from "@zupflow/sdk";
import { cn } from "@/lib/utils";

type Tone = "success" | "warning" | "danger" | "neutral" | "info" | "live";

const TONE: Record<Tone, { dot: string; text: string; bg: string }> = {
  success: { dot: "bg-success", text: "text-success", bg: "bg-success-soft" },
  warning: { dot: "bg-warning", text: "text-sandbox-ink", bg: "bg-warning-soft" },
  danger: { dot: "bg-destructive", text: "text-destructive", bg: "bg-danger-soft" },
  neutral: { dot: "bg-muted-foreground/60", text: "text-muted-foreground", bg: "bg-muted" },
  info: { dot: "bg-brand", text: "text-brand-ink", bg: "bg-brand-soft" },
  live: { dot: "bg-brand animate-pulse", text: "text-brand-ink", bg: "bg-brand-soft" },
};

export function StatusPill({
  tone,
  children,
  className,
}: {
  tone: Tone;
  children: React.ReactNode;
  className?: string;
}) {
  const t = TONE[tone];
  return (
    <span
      className={cn(
        "inline-flex h-[22px] shrink-0 items-center gap-1.5 rounded-full px-2 text-xs font-medium whitespace-nowrap",
        t.bg,
        t.text,
        className,
      )}
    >
      <span aria-hidden className={cn("size-1.5 rounded-full", t.dot)} />
      {children}
    </span>
  );
}

const CONFIG: Record<ConfigurationStatus, [Tone, string]> = {
  ready: ["success", "Ready"],
  incomplete: ["warning", "Incomplete"],
  unconfigured: ["neutral", "Not configured"],
};

export function ConfigStatus({ status, className }: { status: ConfigurationStatus; className?: string }) {
  const [tone, label] = CONFIG[status];
  return (
    <StatusPill tone={tone} className={className}>
      {label}
    </StatusPill>
  );
}

const INSTALLATION: Record<InstallationStatus, [Tone, string]> = {
  connected: ["success", "Connected"],
  reconnect_required: ["danger", "Reconnect required"],
  disconnected: ["neutral", "Disconnected"],
};

export function InstallationStatusPill({ status }: { status: InstallationStatus }) {
  const [tone, label] = INSTALLATION[status];
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

export const RUN_STATUS_LABEL: Record<RunStatus, string> = {
  queued: "Queued",
  running: "Running",
  waiting_for_approval: "Needs approval",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
};

const RUN: Record<RunStatus, Tone> = {
  queued: "neutral",
  running: "live",
  waiting_for_approval: "warning",
  completed: "success",
  failed: "danger",
  cancelled: "neutral",
};

export function RunStatusPill({ status }: { status: RunStatus }) {
  return <StatusPill tone={RUN[status]}>{RUN_STATUS_LABEL[status]}</StatusPill>;
}

const SESSION: Record<ConnectSessionStatus, [Tone, string]> = {
  pending: ["live", "Pending"],
  completed: ["success", "Completed"],
  expired: ["neutral", "Expired"],
  cancelled: ["neutral", "Cancelled"],
  failed: ["danger", "Failed"],
};

export function ConnectSessionStatusPill({ status }: { status: ConnectSessionStatus }) {
  const [tone, label] = SESSION[status];
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

export function AgentStatusPill({ status }: { status: AgentStatus }) {
  return status === "active" ? (
    <StatusPill tone="success">Active</StatusPill>
  ) : (
    <StatusPill tone="neutral">Paused</StatusPill>
  );
}

const EFFECT: Record<ToolEffect, string> = {
  read: "text-muted-foreground border-border",
  write: "text-brand-ink border-brand/40",
  destructive: "text-destructive border-destructive/40",
};

export function ToolEffectTag({ effect }: { effect: ToolEffect }) {
  return (
    <span
      className={cn(
        "inline-flex h-[18px] items-center rounded-[4px] border px-1.5 font-mono text-[10px] uppercase tracking-wider",
        EFFECT[effect],
      )}
    >
      {effect}
    </span>
  );
}
