import { createContext, useContext } from "react";
import { cn } from "@/lib/utils";

/**
 * Set by a layout that already provides the page frame (the workspace page).
 * Pages inside it drop their own container and use a section-sized title.
 */
const NestedPageContext = createContext(false);

export function NestedPage({ children }: { children: React.ReactNode }) {
  return <NestedPageContext.Provider value>{children}</NestedPageContext.Provider>;
}

/** Width and padding for management pages. Full-canvas views own their layout. */
export function PageContainer({
  children,
  className,
  width = "default",
}: {
  children: React.ReactNode;
  className?: string;
  width?: "default" | "narrow" | "wide";
}) {
  const nested = useContext(NestedPageContext);
  if (nested) return <div className={cn(width === "narrow" && "max-w-3xl", className)}>{children}</div>;
  return (
    <div
      className={cn(
        "mx-auto w-full px-4 pt-6 pb-16 sm:px-6 lg:px-8 lg:pt-8",
        width === "narrow" && "max-w-3xl",
        width === "default" && "max-w-6xl",
        width === "wide" && "max-w-7xl",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function PageHeader({
  title,
  description,
  actions,
  eyebrow,
  className,
}: {
  title: React.ReactNode;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  eyebrow?: React.ReactNode;
  className?: string;
}) {
  const nested = useContext(NestedPageContext);
  const Heading = nested ? "h2" : "h1";
  return (
    <header
      className={cn(
        "flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between",
        nested ? "mb-5" : "mb-6 lg:mb-8",
        className,
      )}
    >
      <div className="min-w-0 space-y-1.5">
        {eyebrow ? <div className="flex flex-wrap items-center gap-2">{eyebrow}</div> : null}
        <Heading
          className={cn(
            "leading-tight font-semibold tracking-[-0.02em] text-balance",
            nested ? "text-lg" : "text-[1.6rem]",
          )}
        >
          {title}
        </Heading>
        {description ? <p className="max-w-2xl text-sm text-pretty text-muted-foreground">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
    </header>
  );
}

export function SectionHeader({
  title,
  description,
  actions,
  className,
}: {
  title: React.ReactNode;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("mb-3 flex flex-wrap items-end justify-between gap-3", className)}>
      <div className="min-w-0">
        <h2 className="text-base font-semibold tracking-[-0.01em]">{title}</h2>
        {description ? <p className="mt-0.5 text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {actions ? <div className="flex items-center gap-2">{actions}</div> : null}
    </div>
  );
}

/** A bordered surface. Lighter than Card for dense developer content. */
export function Panel({ children, className, ...props }: React.ComponentProps<"section">) {
  return (
    <section className={cn("rounded-xl border bg-card text-card-foreground", className)} {...props}>
      {children}
    </section>
  );
}

/** Label/value rows for resource details. */
export function Properties({ children, className }: { children: React.ReactNode; className?: string }) {
  return <dl className={cn("divide-y text-sm", className)}>{children}</dl>;
}

export function Property({
  label,
  children,
  className,
}: {
  label: React.ReactNode;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("grid grid-cols-1 gap-1 px-4 py-3 sm:grid-cols-[minmax(9rem,13rem)_1fr] sm:gap-4", className)}>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </div>
  );
}

/** Small uppercase mono label for structural headings inside panels. */
export function Eyebrow({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <span
      className={cn("font-mono text-[10.5px] font-medium tracking-[0.12em] text-muted-foreground uppercase", className)}
    >
      {children}
    </span>
  );
}
