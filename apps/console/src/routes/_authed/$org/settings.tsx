import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { MonoValue } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useUpdateOrganization } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { shortDate } from "@/lib/format";

export const Route = createFileRoute("/_authed/$org/settings")({
  staticData: { crumb: "Settings" },
  component: OrgSettingsPage,
});

const schema = z.object({
  name: z.string().trim().min(1, "Required.").max(60),
  slug: z.string().regex(/^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/, "Use lowercase letters, numbers and hyphens."),
});

function OrgSettingsPage() {
  const org = useOrg();
  useDocumentTitle("Settings", org.name);
  const navigate = useNavigate();
  const update = useUpdateOrganization(org.id);
  const admin = canAdminister(org);
  const form = useForm<z.infer<typeof schema>>({
    resolver: zodResolver(schema),
    values: { name: org.name, slug: org.slug },
  });

  return (
    <PageContainer width="narrow">
      <PageHeader title="Organization settings" />
      <div className="space-y-10">
        <section>
          <SectionHeader title="General" />
          <Panel>
            <Form {...form}>
              <form
                className="space-y-5 p-5"
                onSubmit={form.handleSubmit((values) =>
                  update.mutate(values, {
                    onSuccess: (next) => {
                      toast.success("Saved organization settings");
                      if (next.slug !== org.slug) {
                        void navigate({ to: "/$org/settings", params: { org: next.slug }, replace: true });
                      }
                    },
                    onError: (e) => {
                      if (!applyFieldErrors(form, e)) toastError(e, "Couldn't save");
                    },
                  }),
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
                        <FormDescription>
                          Changing it breaks bookmarked Console links. API calls use IDs and aren't affected.
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
                ) : (
                  <p className="text-sm text-muted-foreground">Only owners and admins can change these.</p>
                )}
              </form>
            </Form>
          </Panel>
        </section>
        <section>
          <SectionHeader title="Identifiers" />
          <Panel>
            <Properties>
              <Property label="Organization ID">
                <MonoValue value={org.id} />
              </Property>
              <Property label="Your role">
                <span className="capitalize">{org.role}</span>
              </Property>
              <Property label="Created">{shortDate(org.created_at)}</Property>
            </Properties>
          </Panel>
        </section>
      </div>
    </PageContainer>
  );
}
