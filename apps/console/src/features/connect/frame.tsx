import { LockIcon } from "lucide-react";
import { ZupflowMark } from "@/components/brand/logo";

/** The page frame shared by every Connect state. */
export function ConnectFrame({ children }: { children: React.ReactNode }) {
  return (
    <main className="flex min-h-svh flex-col items-center justify-center bg-background px-4 py-10">
      <div className="w-full max-w-[400px] rounded-2xl border bg-card p-6 shadow-sm sm:p-8">{children}</div>
      <p className="mt-5 flex items-center gap-1.5 text-xs text-muted-foreground">
        <LockIcon className="size-3" />
        Secured by
        <ZupflowMark className="size-3.5" />
        <span className="font-medium text-foreground">Zupflow</span>
      </p>
    </main>
  );
}

/** A finished Connect state: an icon, what happened, and what to do next. */
export function ConnectOutcome({
  icon,
  title,
  body,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  body: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-3 text-center">
      {icon}
      <h1 className="text-lg font-semibold text-balance">{title}</h1>
      <p className="text-sm text-pretty text-muted-foreground">{body}</p>
      {children}
    </div>
  );
}
