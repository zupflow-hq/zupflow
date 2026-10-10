import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { ZupflowLogo } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { session } from "@/features/auth/session";
import { useCreateOrganization, useViewer } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { slugify } from "@/lib/format";

export const Route = createFileRoute("/_authed/onboarding")({
  component: OnboardingPage,
});

const schema = z.object({
  name: z.string().trim().min(1, "Name your organization.").max(60),
  slug: z.string().regex(/^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/, "Use lowercase letters, numbers and hyphens."),
});

const NEXT_STEPS = [
  ["Add an app", "Register an MCP app once in your organization's catalog."],
  ["Configure each environment", "Sandbox and Production get separate client credentials."],
  ["Create workspaces", "One per customer of your product, each fixed to an environment."],
  ["Connect, grant, run", "Connect an account, grant an agent its tools, start a run."],
] as const;

function OnboardingPage() {
  const navigate = useNavigate();
  const viewer = useViewer();
  const create = useCreateOrganization();
  const form = useForm<z.infer<typeof schema>>({
    resolver: zodResolver(schema),
    defaultValues: { name: "", slug: "" },
  });
  const name = form.watch("name");
  const slugTouched = form.formState.dirtyFields.slug;

  useEffect(() => {
    if (!slugTouched) form.setValue("slug", slugify(name), { shouldValidate: name.length > 0 });
  }, [name, slugTouched, form]);

  const hasOrgs = (viewer.data?.organizations.length ?? 0) > 0;

  return (
    <main className="min-h-svh px-4 py-8 sm:px-10">
      <div className="flex items-center justify-between">
        <ZupflowLogo className="text-[22px]" />
        {hasOrgs ? (
          <Button asChild variant="ghost" size="sm">
            <Link to="/">Cancel</Link>
          </Button>
        ) : session.canSignOut() ? (
          <Button variant="ghost" size="sm" onClick={() => session.signOut()} asChild>
            <Link to="/sign-in">Sign out</Link>
          </Button>
        ) : null}
      </div>

      <div className="mx-auto mt-16 grid max-w-4xl gap-14 lg:mt-24 lg:grid-cols-[1fr_300px]">
        <section>
          <p className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground uppercase">
            {viewer.data ? `Welcome, ${viewer.data.user.name.split(" ")[0]}` : "Welcome"}
          </p>
          <h1 className="mt-2 text-3xl font-semibold tracking-[-0.025em]">Name your organization</h1>
          <p className="mt-2 max-w-md text-sm text-muted-foreground">
            The organization is your company's account. Teammates, apps, API keys and every workspace live inside it.
          </p>

          <Form {...form}>
            <form
              className="mt-8 max-w-md space-y-5"
              onSubmit={form.handleSubmit((values) =>
                create.mutate(values, {
                  onSuccess: (org) => navigate({ to: "/$org", params: { org: org.slug } }),
                  onError: (error) => {
                    if (!applyFieldErrors(form, error)) toastError(error, "Couldn't create the organization");
                  },
                }),
              )}
            >
              <FormField
                control={form.control}
                name="name"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Organization name</FormLabel>
                    <FormControl>
                      <Input placeholder="Northwind" autoFocus autoComplete="organization" {...field} />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name="slug"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Console URL</FormLabel>
                    <FormControl>
                      <InputGroup>
                        <InputGroupAddon>
                          <InputGroupText className="font-mono text-xs">console.zupflow.com/</InputGroupText>
                        </InputGroupAddon>
                        <InputGroupInput className="font-mono text-sm" {...field} />
                      </InputGroup>
                    </FormControl>
                    <FormDescription>You can change this later in settings.</FormDescription>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <Button type="submit" size="lg" disabled={create.isPending}>
                {create.isPending ? <Spinner /> : null}
                Create organization
              </Button>
            </form>
          </Form>
        </section>

        <aside className="border-t pt-8 lg:border-t-0 lg:border-l lg:pt-0 lg:pl-10">
          <p className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground uppercase">Then, in order</p>
          <ol className="mt-4 space-y-5">
            {NEXT_STEPS.map(([title, body], i) => (
              <li key={title} className="flex gap-3">
                <span className="grid size-6 shrink-0 place-items-center rounded-full border font-mono text-[11px] text-muted-foreground">
                  {i + 1}
                </span>
                <div>
                  <p className="text-sm font-medium">{title}</p>
                  <p className="text-sm text-muted-foreground">{body}</p>
                </div>
              </li>
            ))}
          </ol>
        </aside>
      </div>
    </main>
  );
}
