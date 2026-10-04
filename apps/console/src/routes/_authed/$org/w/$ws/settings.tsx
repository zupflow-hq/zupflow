import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LockIcon } from "lucide-react";
import { useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { EnvBadge } from "@/components/app/environment";
import { MonoValue } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useDeleteWorkspace, useUpdateWorkspace } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { shortDate } from "@/lib/format";

export const Route = createFileRoute("/_authed/$org/w/$ws/settings")({
  staticData: { crumb: "Settings" },
  component: WorkspaceSettingsPage,
});

const schema = z.object({
  name: z.string().trim().min(1, "Required.").max(60),
  external_id: z.string().trim().max(120),
});

function WorkspaceSettingsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Settings", ws.name);
  const navigate = useNavigate();
  const update = useUpdateWorkspace(org.id, ws.id);
  const remove = useDeleteWorkspace(org.id);
  const [deleting, setDeleting] = useState(false);
  const admin = canAdminister(org);
  const form = useForm<z.infer<typeof schema>>({
    resolver: zodResolver(schema),
    values: { name: ws.name, external_id: ws.external_id ?? "" },
  });

  return (
    <PageContainer width="narrow">
      <PageHeader title="Workspace settings" />
      <div className="space-y-10">
        <section>
          <SectionHeader title="General" />
          <Panel>
            <Form {...form}>
              <form
                className="space-y-5 p-5"
                onSubmit={form.handleSubmit((values) =>
                  update.mutate(
                    { name: values.name, external_id: values.external_id || null },
                    {
                      onSuccess: () => toast.success("Saved workspace settings"),
                      onError: (e) => {
                        if (!applyFieldErrors(form, e)) toastError(e, "Couldn't save");
                      },
                    },
                  ),
                )}
              >
                <fieldset disabled={!admin} className="space-y-5">
                  <FormField
                    control={form.control}
                    name="name"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>Name</FormLabel>
                        <FormControl>
                          <Input {...field} />
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
                        <FormLabel>External ID</FormLabel>
                        <FormControl>
                          <Input className="font-mono text-sm" {...field} />
                        </FormControl>
                        <FormDescription>
                          How your backend refers to this customer. Zupflow stores it; it never authorizes anything.
                        </FormDescription>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                </fieldset>
                {admin ? (
                  <Button type="submit" disabled={!form.formState.isDirty || update.isPending}>
                    {update.isPending ? <Spinner /> : null}
                    Save
                  </Button>
                ) : null}
              </form>
            </Form>
          </Panel>
        </section>

        <section>
          <SectionHeader title="Environment" />
          <Panel className="flex flex-col gap-3 p-5 sm:flex-row sm:items-center">
            <EnvBadge environment={ws.environment} />
            <p className="flex-1 text-sm text-muted-foreground">
              <LockIcon className="mr-1 inline size-3.5 align-[-2px]" />
              Fixed at creation. Keys, installations, credentials, agents and runs here are bound to it. For the other
              environment, create a separate workspace.
            </p>
          </Panel>
        </section>

        <section>
          <SectionHeader title="Identifiers" />
          <Panel>
            <Properties>
              <Property label="Workspace ID">
                <MonoValue value={ws.id} />
              </Property>
              <Property label="Slug">
                <MonoValue value={ws.slug} />
              </Property>
              <Property label="Created">{shortDate(ws.created_at)}</Property>
            </Properties>
          </Panel>
        </section>

        {org.role === "owner" ? (
          <section>
            <SectionHeader title="Danger zone" />
            <Panel className="flex flex-col gap-4 border-destructive/30 p-5 sm:flex-row sm:items-center">
              <div className="flex-1">
                <p className="font-medium">Delete this workspace</p>
                <p className="text-sm text-muted-foreground">
                  Removes its agents, installations, stored credentials, workspace apps and run history.
                </p>
              </div>
              <Button variant="destructive" onClick={() => setDeleting(true)}>
                Delete workspace
              </Button>
            </Panel>
          </section>
        ) : null}
      </div>

      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={`Delete ${ws.name}?`}
        description={
          <>
            <p>
              This permanently deletes {ws.stats.agents} {ws.stats.agents === 1 ? "agent" : "agents"},{" "}
              {ws.stats.installations} {ws.stats.installations === 1 ? "installation" : "installations"} and every run
              in this {ws.environment} workspace. Credentials are destroyed.
            </p>
            <p>Your backend's API calls for this workspace will start returning 404.</p>
          </>
        }
        confirmText={ws.slug}
        actionLabel="Delete workspace"
        pending={remove.isPending}
        onConfirm={() =>
          remove.mutate(ws.id, {
            onSuccess: () => {
              toast.success(`Deleted ${ws.name}`);
              void navigate({ to: "/$org/workspaces", params: { org: org.slug } });
            },
            onError: (e) => toastError(e, "Couldn't delete the workspace"),
          })
        }
      />
    </PageContainer>
  );
}
