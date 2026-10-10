import { zodResolver } from "@hookform/resolvers/zod";
import { useNavigate } from "@tanstack/react-router";
import { ArrowRightIcon } from "lucide-react";
import { useState } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { safeReturnPath } from "@/lib/oidc";
import type { MockControls } from "./adapter";
import { SignInHeading, SignInLayout } from "./sign-in-layout";

const signUpSchema = z.object({
  name: z.string().trim().min(1, "Enter your name."),
  email: z.string().trim().email("Enter a valid email address."),
});

/** The mock API's simulated sign-in: development builds only. */
export default function MockSignIn({ controls, target }: { controls: MockControls; target: string | undefined }) {
  const navigate = useNavigate();
  const [mode, setMode] = useState<"sign-in" | "sign-up">("sign-in");
  const form = useForm<z.infer<typeof signUpSchema>>({
    resolver: zodResolver(signUpSchema),
    defaultValues: { name: "", email: "" },
  });

  return (
    <SignInLayout footer="Preview build: accounts are simulated in this browser.">
      <SignInHeading
        title={mode === "sign-in" ? "Sign in to the Console" : "Create your account"}
        description={
          mode === "sign-in"
            ? "Manage workspaces, apps and agents for your product."
            : "You'll name your organization next."
        }
      />

      {mode === "sign-in" ? (
        <div className="mt-8 space-y-3">
          <Button
            className="h-11 w-full justify-between"
            onClick={() => {
              controls.signInAsDemo();
              void navigate({ href: safeReturnPath(target) });
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
              controls.signUp(values);
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
    </SignInLayout>
  );
}
