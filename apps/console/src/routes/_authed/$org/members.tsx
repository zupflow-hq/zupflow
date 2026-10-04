import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute } from "@tanstack/react-router";
import type { Member, MemberRole, Organization } from "@zupflow/sdk";
import { Trash2Icon, UserPlusIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { initials } from "@/components/app/shell/user-menu";
import { StatusPill } from "@/components/app/status";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
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
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { canAdminister, useDocumentTitle, useOrg, useViewerUser } from "@/features/context";
import { useInviteMember, useMembers, useRemoveMember, useUpdateMember } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/members")({
  staticData: { crumb: "Members" },
  component: MembersPage,
});

const ROLES: { value: MemberRole; label: string; body: string }[] = [
  { value: "owner", label: "Owner", body: "Everything, including roles and deleting workspaces." },
  { value: "admin", label: "Admin", body: "Apps, configurations, workspaces, keys and members." },
  { value: "developer", label: "Developer", body: "Agents, installations, runs and Sandbox keys." },
];

function MembersPage() {
  const org = useOrg();
  const me = useViewerUser();
  useDocumentTitle("Members", org.name);
  const members = useMembers(org.id);
  const updateRole = useUpdateMember(org.id);
  const remove = useRemoveMember(org.id);
  const [inviting, setInviting] = useState(false);
  const [removing, setRemoving] = useState<Member | null>(null);
  const admin = canAdminister(org);

  return (
    <PageContainer>
      <PageHeader
        title="Members"
        description="People who can sign in to this Console. Your product's end users are never members — they only see Connect."
        actions={
          admin ? (
            <Button onClick={() => setInviting(true)}>
              <UserPlusIcon />
              Invite member
            </Button>
          ) : null
        }
      />
      {members.isError ? (
        <QueryError error={members.error} onRetry={() => members.refetch()} what="members" />
      ) : (
        <Panel className="overflow-hidden">
          {!members.data ? (
            <RowsSkeleton rows={4} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="pl-4">Member</TableHead>
                  <TableHead>Role</TableHead>
                  <TableHead className="hidden md:table-cell">Last active</TableHead>
                  <TableHead className="pr-4 text-right">
                    <span className="sr-only">Actions</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {members.data.map((m) => {
                  const isMe = m.user.id === me.id;
                  return (
                    <TableRow key={m.id} className="hover:bg-transparent">
                      <TableCell className="w-full max-w-0 py-3 pl-4">
                        <div className="flex items-center gap-3">
                          <Avatar className="size-8">
                            <AvatarFallback className="text-xs font-semibold">{initials(m.user.name)}</AvatarFallback>
                          </Avatar>
                          <div className="min-w-0">
                            <p className="flex min-w-0 items-center gap-2 font-medium">
                              <span className="truncate">{m.status === "invited" ? m.user.email : m.user.name}</span>
                              {isMe ? <span className="text-xs font-normal text-muted-foreground">You</span> : null}
                              {m.status === "invited" ? <StatusPill tone="info">Invited</StatusPill> : null}
                            </p>
                            {m.status === "active" ? (
                              <p className="truncate text-xs text-muted-foreground">{m.user.email}</p>
                            ) : null}
                          </div>
                        </div>
                      </TableCell>
                      <TableCell>
                        {org.role === "owner" && !isMe ? (
                          <RoleSelect
                            value={m.role}
                            onChange={(role) =>
                              updateRole.mutate(
                                { memberId: m.id, role },
                                {
                                  onSuccess: () => toast.success(`${m.user.name} is now ${role}`),
                                  onError: (e) => toastError(e, "Couldn't change the role"),
                                },
                              )
                            }
                          />
                        ) : (
                          <span className="capitalize">{m.role}</span>
                        )}
                      </TableCell>
                      <TableCell className="hidden text-muted-foreground md:table-cell">
                        {m.status === "invited" ? (
                          <>
                            Invited <RelativeTime iso={m.invited_at} />
                          </>
                        ) : (
                          <RelativeTime iso={m.last_active_at} />
                        )}
                      </TableCell>
                      <TableCell className="pr-4 text-right">
                        {admin && !isMe ? (
                          <Button
                            variant="ghost"
                            size="sm"
                            className="text-destructive hover:text-destructive max-sm:size-8 max-sm:p-0"
                            aria-label={m.status === "invited" ? "Cancel invite" : `Remove ${m.user.name}`}
                            onClick={() => setRemoving(m)}
                          >
                            <Trash2Icon className="sm:hidden" />
                            <span className="hidden sm:inline">
                              {m.status === "invited" ? "Cancel invite" : "Remove"}
                            </span>
                          </Button>
                        ) : null}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </Panel>
      )}
      <InviteDialog org={org} open={inviting} onOpenChange={setInviting} />
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={
          removing?.status === "invited"
            ? `Cancel the invite for ${removing.user.email}?`
            : `Remove ${removing?.user.name}?`
        }
        description={<p>They lose Console access immediately. Their past actions stay in the audit log.</p>}
        actionLabel={removing?.status === "invited" ? "Cancel invite" : "Remove member"}
        pending={remove.isPending}
        onConfirm={() =>
          removing &&
          remove.mutate(removing.id, {
            onSuccess: () => {
              toast.success(removing.status === "invited" ? "Invite cancelled" : `Removed ${removing.user.name}`);
              setRemoving(null);
            },
            onError: (e) => toastError(e),
          })
        }
      />
    </PageContainer>
  );
}

function RoleSelect({ value, onChange }: { value: MemberRole; onChange: (role: MemberRole) => void }) {
  return (
    <Select value={value} onValueChange={(v) => onChange(v as MemberRole)}>
      <SelectTrigger size="sm" className="w-28 sm:w-32" aria-label="Role">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {ROLES.map((r) => (
          <SelectItem key={r.value} value={r.value}>
            {r.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

const inviteSchema = z.object({
  email: z.string().trim().email("Enter a valid email address."),
  role: z.enum(["owner", "admin", "developer"]),
});

function InviteDialog({
  org,
  open,
  onOpenChange,
}: {
  org: Organization;
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const invite = useInviteMember(org.id);
  const form = useForm<z.infer<typeof inviteSchema>>({
    resolver: zodResolver(inviteSchema),
    defaultValues: { email: "", role: "developer" },
  });
  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Invite to {org.name}</DialogTitle>
          <DialogDescription>They'll join when they sign up with this address.</DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="invite-form"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) =>
              invite.mutate(values, {
                onSuccess: () => {
                  toast.success(`Invited ${values.email}`);
                  onOpenChange(false);
                },
                onError: (e) => {
                  if (!applyFieldErrors(form, e)) toastError(e, "Couldn't send the invite");
                },
              }),
            )}
          >
            <FormField
              control={form.control}
              name="email"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Email</FormLabel>
                  <FormControl>
                    <Input type="email" autoComplete="off" placeholder="teammate@company.com" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="role"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Role</FormLabel>
                  <RadioGroup
                    value={field.value}
                    onValueChange={field.onChange}
                    className="gap-0 divide-y rounded-lg border"
                  >
                    {ROLES.filter((r) => r.value !== "owner" || org.role === "owner").map((r) => (
                      <Label
                        key={r.value}
                        htmlFor={`role-${r.value}`}
                        className="flex cursor-pointer items-start gap-3 p-3 font-normal has-[[data-state=checked]]:bg-accent/50"
                      >
                        <RadioGroupItem id={`role-${r.value}`} value={r.value} className="mt-0.5" />
                        <span>
                          <span className="block text-sm font-medium">{r.label}</span>
                          <span className="block text-xs text-muted-foreground">{r.body}</span>
                        </span>
                      </Label>
                    ))}
                  </RadioGroup>
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
          <Button type="submit" form="invite-form" disabled={invite.isPending}>
            {invite.isPending ? <Spinner /> : null}
            Send invite
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
