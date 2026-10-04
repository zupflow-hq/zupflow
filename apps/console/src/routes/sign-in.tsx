import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { ArrowRightIcon } from "lucide-react";
import { useState } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { ZupflowLogo, ZupflowMark } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { session } from "@/features/auth/session";

export const Route = createFileRoute("/sign-in")({
  validateSearch: z.object({ redirect: z.string().optional() }),
  beforeLoad: () => {
    if (session.isSignedIn()) throw redirect({ to: "/" });
  },
  component: SignInPage,
});

const signUpSchema = z.object({
  name: z.string().trim().min(1, "Enter your name."),
  email: z.string().trim().email("Enter a valid email address."),
});

function SignInPage() {
  const navigate = useNavigate();
  const { redirect: target } = Route.useSearch();
  const [mode, setMode] = useState<"sign-in" | "sign-up">("sign-in");
  const form = useForm<z.infer<typeof signUpSchema>>({
    resolver: zodResolver(signUpSchema),
    defaultValues: { name: "", email: "" },
  });

  const proceed = () => {
    // Only follow same-origin, in-app redirects.
    if (target?.startsWith("/") && !target.startsWith("//")) {
      void navigate({ href: target });
    } else {
      void navigate({ to: "/" });
    }
  };

  return (
    <main className="grid min-h-svh lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)]">
      <section className="flex flex-col px-4 py-8 sm:px-10">
        <ZupflowLogo className="text-[22px]" />
        <div className="mx-auto flex w-full max-w-[360px] flex-1 flex-col justify-center py-12">
          <h1 className="text-2xl font-semibold tracking-[-0.02em]">
            {mode === "sign-in" ? "Sign in to the Console" : "Create your account"}
          </h1>
          <p className="mt-1.5 text-sm text-muted-foreground">
            {mode === "sign-in"
              ? "Manage workspaces, apps and agents for your product."
              : "You'll name your organization next."}
          </p>

          {mode === "sign-in" ? (
            <div className="mt-8 space-y-3">
              <Button
                className="h-11 w-full justify-between"
                onClick={() => {
                  session.signInAsDemo();
                  proceed();
                }}
              >
                <span className="flex items-center gap-2.5">
                  <span className="grid size-6 place-items-center rounded-full bg-primary-foreground/15 text-[11px] font-semibold">
                    JL
                  </span>
                  Continue as Jordan Lee
                </span>
                <ArrowRightIcon />
              </Button>
              <p className="text-center text-xs text-muted-foreground">
                Owner of the <span className="font-medium text-foreground">Northwind</span> demo organization
              </p>
              <div className="flex items-center gap-3 py-3">
                <Separator className="flex-1" />
                <span className="text-xs text-muted-foreground">or</span>
                <Separator className="flex-1" />
              </div>
              <Button variant="outline" className="h-11 w-full" onClick={() => setMode("sign-up")}>
                Create a new account
              </Button>
            </div>
          ) : (
            <Form {...form}>
              <form
                className="mt-8 space-y-4"
                onSubmit={form.handleSubmit((values) => {
                  session.signUp(values);
                  void navigate({ to: "/onboarding" });
                })}
              >
                <FormField
                  control={form.control}
                  name="name"
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Name</FormLabel>
                      <FormControl>
                        <Input autoComplete="name" autoFocus {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
                <FormField
                  control={form.control}
                  name="email"
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Work email</FormLabel>
                      <FormControl>
                        <Input type="email" autoComplete="email" {...field} />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />
                <Button type="submit" className="h-11 w-full">
                  Create account
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  className="w-full text-muted-foreground"
                  onClick={() => setMode("sign-in")}
                >
                  I already have an account
                </Button>
              </form>
            </Form>
          )}
        </div>
        <p className="text-xs text-muted-foreground">Preview build: accounts are simulated in this browser.</p>
      </section>

      <aside className="relative hidden overflow-hidden bg-[#10191D] text-white lg:flex lg:flex-col lg:justify-between lg:p-12">
        <ZupflowMark
          className="pointer-events-none absolute -right-24 -bottom-24 size-[520px] opacity-[0.07]"
          loop="#35C8F5"
        />
        <p className="font-mono text-[11px] tracking-[0.14em] text-white/50 uppercase">
          Personal agents, inside your product
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
