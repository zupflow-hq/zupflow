import { zodResolver } from "@hookform/resolvers/zod";
import { useNavigate } from "@tanstack/react-router";
import type { Environment, Organization } from "@zupflow/sdk";
import { LockIcon } from "lucide-react";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { useCreateWorkspace } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { slugify } from "@/lib/format";

const schema = z.object({
  name: z.string().trim().min(1, "Name the workspace.").max(60),
  slug: z.string().regex(/^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/, "Use lowercase letters, numbers and hyphens."),
  external_id: z.string().trim().max(120),
});
type Values = z.infer<typeof schema>;

const ENV_COPY: Record<Environment, string> = {
  sandbox: "For testing. It uses Sandbox keys and Sandbox app configurations.",
  production: "For a live customer. It uses Production keys and Production app configurations.",
};

/** Creates a workspace in the environment the Console is in. */
export function CreateWorkspaceDialog({
  org,
  environment,
  open,
  onOpenChange,
}: {
  org: Organization;
  environment: Environment;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const create = useCreateWorkspace(org.id);
  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: { name: "", slug: "", external_id: "" },
  });
  const name = form.watch("name");
  const slugTouched = form.formState.dirtyFields.slug;

  useEffect(() => {
    if (slugTouched) return;
    const base = slugify(name);
    const suffix = environment === "sandbox" && base ? "-sandbox" : "";
    form.setValue("slug", `${base}${suffix}`.slice(0, 40));
  }, [name, environment, slugTouched, form]);

  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create workspace</DialogTitle>
          <DialogDescription>
            An isolated boundary for one of your customers: its own agents, installations, credentials and runs.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="create-workspace"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) =>
              create.mutate(
                { ...values, environment, external_id: values.external_id || null },
                {
                  onSuccess: (ws) => {
                    toast.success(`Created ${ws.name}`);
                    onOpenChange(false);
                    void navigate({ to: "/$org/w/$ws", params: { org: org.slug, ws: ws.id } });
                  },
                  onError: (error) => {
                    if (!applyFieldErrors(form, error)) toastError(error, "Couldn't create the workspace");
                  },
                },
              ),
            )}
          >
            <div className="flex gap-3 rounded-lg border bg-muted/30 p-3">
              <EnvBadge environment={environment} className="mt-0.5" />
              <p className="text-xs text-muted-foreground">
                {ENV_COPY[environment]}{" "}
                <span className="inline-flex items-center gap-1">
                  <LockIcon className="size-3" />
                  Fixed once created.
                </span>
              </p>
            </div>
            <FormField
              control={form.control}
              name="name"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Name</FormLabel>
                  <FormControl>
                    <Input placeholder="Acme" autoComplete="off" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <div className="grid gap-4 sm:grid-cols-2">
              <FormField
                control={form.control}
                name="slug"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Slug</FormLabel>
                    <FormControl>
                      <Input className="font-mono text-sm" autoComplete="off" {...field} />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name="external_id"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>
                      External ID <span className="font-normal text-muted-foreground">(optional)</span>
                    </FormLabel>
                    <FormControl>
                      <Input className="font-mono text-sm" placeholder="tenant_8c1f2a" autoComplete="off" {...field} />
                    </FormControl>
                    <FormDescription>This customer's ID in your platform.</FormDescription>
                    <FormMessage />
                  </FormItem>
                )}
              />
            </div>
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="create-workspace" disabled={create.isPending}>
            {create.isPending ? <Spinner /> : null}
            Create workspace
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
