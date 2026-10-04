import type { AppIcon as AppIconData } from "@zupflow/sdk";
import { AlertTriangleIcon, CheckIcon, CopyIcon, RotateCwIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { errorMessage, isStatus } from "@/lib/errors";
import { fullDateTime, relative } from "@/lib/format";
import { cn } from "@/lib/utils";

export function useCopy(timeout = 1600) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), timeout);
    return () => clearTimeout(t);
  }, [copied, timeout]);
  return {
    copied,
    copy: async (value: string) => {
      try {
        await navigator.clipboard.writeText(value);
        setCopied(true);
      } catch {
        // Clipboard blocked; the value stays selectable.
      }
    },
  };
}

export function CopyButton({
  value,
  label = "Copy",
  className,
  size = "icon-xs",
}: {
  value: string;
  label?: string;
  className?: string;
  size?: "icon-xs" | "icon-sm";
}) {
  const { copied, copy } = useCopy();
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size={size}
          className={cn("text-muted-foreground hover:text-foreground", className)}
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            void copy(value);
          }}
          aria-label={copied ? "Copied" : label}
        >
          {copied ? <CheckIcon className="text-success" /> : <CopyIcon />}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{copied ? "Copied" : label}</TooltipContent>
    </Tooltip>
  );
}

/** An identifier or value in mono, with a copy control. */
export function MonoValue({
  value,
  display,
  className,
  copy = true,
}: {
  value: string;
  display?: string;
  className?: string;
  copy?: boolean;
}) {
  return (
    <span className={cn("inline-flex min-w-0 max-w-full items-center gap-0.5", className)}>
      <code className="truncate rounded bg-muted px-1.5 py-0.5 text-[12px] text-foreground/90">{display ?? value}</code>
      {copy ? <CopyButton value={value} label="Copy" /> : null}
    </span>
  );
}

/** Relative time that stays current, with the exact time on hover. */
export function RelativeTime({
  iso,
  fallback = "Never",
  className,
}: {
  iso: string | null | undefined;
  fallback?: string;
  className?: string;
}) {
  const [, tick] = useState(0);
  useEffect(() => {
    if (!iso) return;
    const t = setInterval(() => tick((n) => n + 1), 30_000);
    return () => clearInterval(t);
  }, [iso]);
  if (!iso) return <span className={cn("text-muted-foreground", className)}>{fallback}</span>;
  return (
    <time dateTime={iso} title={fullDateTime(iso)} className={cn("whitespace-nowrap", className)}>
      {relative(iso)}
    </time>
  );
}

export function AppIcon({
  icon,
  size = "default",
  className,
}: {
  icon: AppIconData;
  size?: "sm" | "default" | "lg";
  className?: string;
}) {
  return (
    <span
      aria-hidden
      style={{ backgroundColor: icon.color }}
      className={cn(
        "inline-flex shrink-0 items-center justify-center font-mono font-semibold text-white shadow-[inset_0_0_0_1px_rgb(255_255_255/0.12)]",
        size === "sm" && "size-6 rounded-[5px] text-[10px]",
        size === "default" && "size-8 rounded-md text-[12px]",
        size === "lg" && "size-12 rounded-lg text-base",
        className,
      )}
    >
      {icon.glyph}
    </span>
  );
}

export function QueryError({
  error,
  onRetry,
  className,
  what = "this",
}: {
  error: unknown;
  onRetry?: () => void;
  className?: string;
  what?: string;
}) {
  const notFound = isStatus(error, 404);
  return (
    <div
      role="alert"
      className={cn(
        "flex flex-col items-start gap-3 rounded-xl border border-destructive/30 bg-danger-soft/50 p-4 text-sm sm:flex-row sm:items-center",
        className,
      )}
    >
      <AlertTriangleIcon className="size-4 shrink-0 text-destructive" />
      <div className="flex-1">
        <p className="font-medium">{notFound ? `We couldn't find ${what}.` : `Couldn't load ${what}.`}</p>
        <p className="text-muted-foreground">
          {notFound ? "It may have been deleted, or it belongs to a workspace you can't access." : errorMessage(error)}
        </p>
      </div>
      {onRetry && !notFound ? (
        <Button variant="outline" size="sm" onClick={onRetry}>
          <RotateCwIcon />
          Retry
        </Button>
      ) : null}
    </div>
  );
}

/** Row skeletons shaped like a table's content. */
export function RowsSkeleton({
  rows = 5,
  columns = [40, 18, 14, 16],
  className,
}: {
  rows?: number;
  columns?: number[];
  className?: string;
}) {
  return (
    <div className={cn("divide-y", className)} role="status" aria-busy aria-label="Loading">
      {Array.from({ length: rows }, (_, r) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: static placeholder rows
        <div key={r} className="flex h-14 items-center gap-6 px-4">
          {columns.map((w, c) => (
            <Skeleton
              // biome-ignore lint/suspicious/noArrayIndexKey: static placeholder
              key={c}
              className={cn("h-3.5", c === 0 ? "flex-none" : "hidden flex-none sm:block")}
              style={{ width: `${Math.max(8, w - ((r * 7 + c * 3) % 9))}%` }}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

export function JsonBlock({ value, className }: { value: unknown; className?: string }) {
  const text = JSON.stringify(value, null, 2) ?? "null";
  return (
    <div className={cn("group relative", className)}>
      <pre className="max-h-80 overflow-auto rounded-lg border bg-muted/50 p-3 text-[12px] leading-relaxed">
        <code>{text}</code>
      </pre>
      <CopyButton
        value={text}
        label="Copy JSON"
        className="absolute top-1.5 right-1.5 bg-card opacity-0 shadow-xs group-hover:opacity-100 focus-visible:opacity-100"
      />
    </div>
  );
}

/** Keyboard-accessible clickable row for tables that navigate. */
export function rowLinkProps(onOpen: () => void) {
  return {
    role: "link" as const,
    tabIndex: 0,
    onClick: (e: React.MouseEvent) => {
      if ((e.target as HTMLElement).closest("button, a, [role=menuitem]")) return;
      onOpen();
    },
    onKeyDown: (e: React.KeyboardEvent) => {
      if (e.key === "Enter") onOpen();
    },
    className:
      "cursor-pointer outline-none focus-visible:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/40 focus-visible:ring-inset",
  };
}
