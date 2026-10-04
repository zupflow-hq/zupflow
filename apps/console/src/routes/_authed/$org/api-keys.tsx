import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute } from "@tanstack/react-router";
import type { ApiKey, Environment, OrganizationId } from "@zupflow/sdk";
import { KeyRoundIcon, PlusIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { ENVIRONMENT_LABEL, EnvBadge } from "@/components/app/environment";
import { QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { SecretRevealDialog } from "@/components/app/secret-reveal";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useApiKeys, useCreateApiKey, useRevokeApiKey } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/_authed/$org/api-keys")({
  staticData: { crumb: "API keys" },
  component: ApiKeysPage,
});

function ApiKeysPage() {
  const org = useOrg();
  useDocumentTitle("API keys", org.name);
  const keys = useApiKeys(org.id);
  const revoke = useRevokeApiKey(org.id);
  const [creating, setCreating] = useState(false);
  const [secret, setSecret] = useState<{ value: string; key: ApiKey } | null>(null);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  const environment = useEnvironment();
  const mayManage = environment === "sandbox" || canAdminister(org);

  return (
    <PageContainer>
      <PageHeader
        title="API keys"
        description={`Keys for calling the Zupflow API from your servers. These are ${ENVIRONMENT_LABEL[environment]} keys: they're refused for anything in the other environment.`}
        actions={
          mayManage ? (
            <Button onClick={() => setCreating(true)}>
              <PlusIcon />
              Create {ENVIRONMENT_LABEL[environment]} key
            </Button>
          ) : null
        }
      />
      {keys.isError ? (
        <QueryError error={keys.error} onRetry={() => keys.refetch()} what="API keys" />
      ) : (
        <KeySection
          environment={environment}
          keys={keys.data?.filter((k) => k.environment === environment)}
          onRevoke={setRevoking}
          canRevoke={mayManage}
        />
      )}

      <CreateKeyDialog
        orgId={org.id}
        environment={environment}
        open={creating}
        onOpenChange={setCreating}
        onCreated={(value, key) => {
          setCreating(false);
          setSecret({ value, key });
        }}
      />
      <SecretRevealDialog
        title={secret ? `${secret.key.name} created` : ""}
        description={
          secret ? (
            <span className="inline-flex flex-wrap items-center gap-1.5">
              A <EnvBadge environment={secret.key.environment} size="sm" /> key. Use it from your servers only — never
              in a browser or app.
            </span>
          ) : null
        }
        secret={secret?.value ?? null}
        onClose={() => setSecret(null)}
      />
      <ConfirmDialog
        open={revoking !== null}
        onOpenChange={(o) => !o && setRevoking(null)}
        title={`Revoke ${revoking?.name}?`}
        description={<p>Requests using this key start failing with 401 right away. This can't be undone.</p>}
        actionLabel="Revoke key"
        pending={revoke.isPending}
        onConfirm={() =>
          revoking &&
          revoke.mutate(revoking.id, {
            onSuccess: () => {
              toast.success(`Revoked ${revoking.name}`);
              setRevoking(null);
            },
            onError: (e) => toastError(e, "Couldn't revoke"),
          })
        }
      />
    </PageContainer>
  );
}

function KeySection({
  environment,
  keys,
  onRevoke,
  canRevoke,
}: {
  environment: Environment;
  keys: ApiKey[] | undefined;
  onRevoke: (key: ApiKey) => void;
  canRevoke: boolean;
}) {
  return (
    <section>
      <p className="mb-3 text-sm text-muted-foreground">
        {environment === "production"
          ? "Production keys start with zf_live_. Owners and admins create and revoke them."
          : "Sandbox keys start with zf_test_. Any member can create them."}
      </p>
      <Panel className="overflow-hidden">
        {!keys ? (
          <RowsSkeleton rows={2} />
        ) : keys.length === 0 ? (
          <div className="flex flex-col items-center gap-2 p-8 text-center">
            <KeyRoundIcon className="size-5 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">No {ENVIRONMENT_LABEL[environment]} keys yet.</p>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead className="pl-4">Name</TableHead>
                <TableHead className="hidden sm:table-cell">Key</TableHead>
                <TableHead className="hidden md:table-cell">Created by</TableHead>
                <TableHead className="hidden sm:table-cell">Last used</TableHead>
                <TableHead className="pr-4 text-right">
                  <span className="sr-only">Actions</span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {keys.map((k) => (
                <TableRow key={k.id} className={cn("hover:bg-transparent", k.revoked_at && "text-muted-foreground")}>
                  <TableCell className="py-3 pl-4">
                    <p className="font-medium">{k.name}</p>
                    <code className={cn("text-[11.5px] sm:hidden", k.revoked_at && "line-through")}>
                      {k.prefix}…{k.last4}
                    </code>
                    <p className="text-xs text-muted-foreground">
                      Created <RelativeTime iso={k.created_at} />
                    </p>
                  </TableCell>
                  <TableCell className="hidden sm:table-cell">
                    <code className={cn("text-[12px]", k.revoked_at && "line-through")}>
                      {k.prefix}…{k.last4}
                    </code>
                  </TableCell>
                  <TableCell className="hidden md:table-cell">{k.created_by.name}</TableCell>
                  <TableCell className="hidden sm:table-cell">
                    <RelativeTime iso={k.last_used_at} />
                  </TableCell>
                  <TableCell className="pr-4 text-right">
                    {k.revoked_at ? (
                      <StatusPill tone="neutral">Revoked</StatusPill>
                    ) : canRevoke ? (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="text-destructive hover:text-destructive"
                        onClick={() => onRevoke(k)}
                      >
                        Revoke
                      </Button>
                    ) : null}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </Panel>
    </section>
  );
}

const keySchema = z.object({
  name: z.string().trim().min(1, "Name the key after where it's used.").max(60),
});

function CreateKeyDialog({
  orgId,
  environment,
  open,
  onOpenChange,
  onCreated,
}: {
  orgId: OrganizationId;
  environment: Environment;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (secret: string, key: ApiKey) => void;
}) {
  const create = useCreateApiKey(orgId);
  const form = useForm<z.infer<typeof keySchema>>({
    resolver: zodResolver(keySchema),
    defaultValues: { name: "" },
  });
  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Create {ENVIRONMENT_LABEL[environment]} key</DialogTitle>
          <DialogDescription>The secret is shown once, right after creation.</DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="create-key"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) =>
              create.mutate(
                { ...values, environment },
                {
                  onSuccess: ({ key, secret }) => onCreated(secret, key),
                  onError: (e) => {
                    if (!applyFieldErrors(form, e)) toastError(e, "Couldn't create the key");
                  },
                },
              ),
            )}
          >
            <div className="flex items-center gap-3 rounded-lg border bg-muted/30 p-3 text-xs text-muted-foreground">
              <EnvBadge environment={environment} size="sm" />
              Starts with <code>{environment === "production" ? "zf_live_" : "zf_test_"}</code>
            </div>
            <FormField
              control={form.control}
              name="name"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Name</FormLabel>
                  <FormControl>
                    <Input placeholder="Billing worker" autoComplete="off" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="create-key" disabled={create.isPending}>
            {create.isPending ? <Spinner /> : null}
            Create key
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
