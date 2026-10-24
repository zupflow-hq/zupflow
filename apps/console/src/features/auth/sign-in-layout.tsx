import type { ReactNode } from "react";
import { ZupflowLogo, ZupflowMark } from "@/components/brand/logo";
import { Skeleton } from "@/components/ui/skeleton";

/** The sign-in screens' frame: the form on the left, the product on the right from `lg` up. */
export function SignInLayout({ children, footer }: { children: ReactNode; footer?: ReactNode }) {
  return (
    <main className="grid min-h-svh lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)]">
      <section className="flex flex-col px-4 py-8 sm:px-10">
        <ZupflowLogo className="text-[22px]" />
        <div className="mx-auto flex w-full max-w-[360px] flex-1 flex-col justify-center py-12">{children}</div>
        {footer ? <p className="text-xs text-muted-foreground">{footer}</p> : null}
      </section>

      <aside className="relative hidden overflow-hidden bg-[#10191D] text-white lg:flex lg:flex-col lg:justify-between lg:p-12">
        <ZupflowMark
          className="pointer-events-none absolute -right-24 -bottom-24 size-[520px] opacity-[0.07]"
          loop="#35C8F5"
        />
        <p className="font-mono text-[11px] tracking-[0.14em] text-white/50 uppercase">
          AI agents, inside your product
        </p>
        <div className="relative max-w-md space-y-6">
          <p className="text-[28px] leading-[1.15] font-semibold tracking-[-0.02em] text-balance">
            Give each customer an isolated workspace, their own connected accounts, and agents that only touch what you
            grant.
          </p>
          <ol className="space-y-2.5 font-mono text-[12.5px] text-white/70">
            {[
              ["POST", "/v1/organizations/{org}/workspaces"],
              ["POST", "/v1/workspaces/{ws}/connect-sessions"],
              ["POST", "/v1/workspaces/{ws}/agents/{agent}/grants"],
              ["POST", "/v1/workspaces/{ws}/agents/{agent}/runs"],
            ].map(([method, path]) => (
              <li key={path} className="flex gap-3">
                <span className="w-10 text-[#35C8F5]">{method}</span>
                <span className="truncate">{path}</span>
              </li>
            ))}
          </ol>
        </div>
        <div className="flex gap-2">
          <span className="chamfer bg-[#35C8F5]/15 px-2 py-0.5 font-mono text-[10.5px] tracking-[0.08em] text-[#6FD8F8] uppercase">
            Production
          </span>
          <span className="chamfer bg-[#E8AA3C]/15 px-2 py-0.5 font-mono text-[10.5px] tracking-[0.08em] text-[#F0C26E] uppercase">
            Sandbox
          </span>
        </div>
      </aside>
    </main>
  );
}

/** The provider's host, which is what people recognize: `id.example.com`, not its whole issuer URL. */
export function providerName(issuer: string): string {
  try {
    return new URL(issuer).host;
  } catch {
    return issuer;
  }
}

export function SignInHeading({ title, description }: { title: string; description: string }) {
  return (
    <>
      <h1 className="text-2xl font-semibold tracking-[-0.02em]">{title}</h1>
      <p className="mt-1.5 text-sm text-muted-foreground">{description}</p>
    </>
  );
}

/** A sign-in panel still loading: heading, description, the button and its note. */
export function SignInSkeleton({ label = "Loading" }: { label?: string }) {
  return (
    <SignInLayout>
      <div role="status" aria-busy="true" aria-label={label} className="space-y-3">
        <Skeleton className="h-8 w-56" />
        <Skeleton className="h-4 w-72 max-w-full" />
        <Skeleton className="mt-8 h-11 w-full" />
        <Skeleton className="mx-auto h-3 w-44" />
      </div>
    </SignInLayout>
  );
}
