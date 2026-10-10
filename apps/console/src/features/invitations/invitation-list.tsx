import { useNavigate } from "@tanstack/react-router";
import { type Invitation, newIdempotencyKey, type Organization } from "@zupflow/sdk";
import { MailIcon, MailWarningIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useAcceptInvitation, useDeclineInvitation, useInvitations } from "@/features/queries";
import { isApiError, isStatus, toastError } from "@/lib/errors";
import { cn } from "@/lib/utils";

/** Invitations the signed-in person can still accept. */
export function usePendingInvitations() {
  const invitations = useInvitations();
  return (invitations.data ?? []).filter((i) => i.status === "pending");
}

const ARTICLE: Record<Invitation["role"], string> = {
  owner: "an owner",
  admin: "an admin",
  developer: "a developer",
  viewer: "a viewer",
};

function InvitationRow({ invitation, onJoined }: { invitation: Invitation; onJoined?: () => void }) {
  const navigate = useNavigate();
  const accept = useAcceptInvitation();
  const decline = useDeclineInvitation();
  const [confirmingDecline, setConfirmingDecline] = useState(false);
  const busy = accept.isPending || decline.isPending;
  const expired = invitation.status === "expired";
  const org = invitation.organization;
  const inviter = invitation.invited_by;

  const goTo = async (slug: string) => {
    onJoined?.();
    await navigate({ to: "/$org", params: { org: slug } });
  };

  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-3 py-3.5">
      <span
        aria-hidden
        className="grid size-9 shrink-0 place-items-center rounded-md bg-muted font-mono text-[13px] font-semibold"
      >
        {org.name.slice(0, 1).toUpperCase()}
      </span>
      <div className="min-w-0 flex-1 basis-40">
        <p className="flex min-w-0 items-center gap-2 font-medium">
          <span className="truncate">{org.name}</span>
          {expired ? <StatusPill tone="neutral">Expired</StatusPill> : null}
        </p>
        <p className="text-xs text-muted-foreground">
          {expired ? (
            <>
              Expired <RelativeTime iso={invitation.expires_at} />. Ask {inviter.name} to invite you again.
            </>
          ) : (
            <>
              {inviter.name} invited you as {ARTICLE[invitation.role]} · <RelativeTime iso={invitation.invited_at} />
            </>
          )}
        </p>
        {/* The address, so a lookalike name can be told from the real sender. */}
        <p className="truncate text-xs text-muted-foreground" title={inviter.email}>
          From {inviter.email}
        </p>
      </div>
      <div className="flex w-full gap-2 sm:w-auto">
        <Button
          variant="outline"
          size="sm"
          className="flex-1 sm:flex-none"
          disabled={busy}
          aria-label={`Decline the invitation to ${org.name}`}
          onClick={() => setConfirmingDecline(true)}
        >
          Decline
        </Button>
        {expired ? null : (
          <Button
            size="sm"
            className="flex-1 sm:flex-none"
            disabled={busy}
            aria-label={`Accept and join ${org.name}`}
            onClick={async () => {
              // Awaited rather than through `mutate`'s callbacks: the row leaves the list, and unmounts, as it settles.
              let joined: Organization;
              try {
                joined = await accept.mutateAsync({ invitationId: invitation.id, idempotencyKey: newIdempotencyKey() });
              } catch (error) {
                if (isApiError(error, "invitation_accepted")) {
                  // Accepted already, from another tab or device: they're a member, so go there.
                  toast.info(`You're already a member of ${org.name}`);
                  await goTo(org.slug);
                } else if (isStatus(error, 404)) {
                  toast.error(`The invitation to ${org.name} was withdrawn`, {
                    description: "Ask an admin of the organization to invite you again.",
                  });
                } else {
                  toastError(error, "Couldn't accept the invitation");
                }
                return;
              }
              toast.success(`You joined ${joined.name}`);
              await goTo(joined.slug);
            }}
          >
            {accept.isPending ? <Spinner /> : null}
            Accept
          </Button>
        )}
      </div>
      <ConfirmDialog
        open={confirmingDecline}
        onOpenChange={setConfirmingDecline}
        title={`Decline the invitation to ${org.name}?`}
        description={<p>It's withdrawn, and you can't accept it afterwards. They can invite you again.</p>}
        actionLabel="Decline"
        pending={decline.isPending}
        onConfirm={async () => {
          try {
            await decline.mutateAsync(invitation.id);
            toast.success(`Declined the invitation to ${org.name}`);
          } catch (error) {
            if (isStatus(error, 404)) {
              // Withdrawn meanwhile: what they wanted.
              toast.success(`The invitation to ${org.name} is gone`);
            } else {
              toastError(error, "Couldn't decline the invitation");
            }
          } finally {
            setConfirmingDecline(false);
          }
        }}
      />
    </li>
  );
}

function RowSkeleton() {
  return (
    <li className="flex items-center gap-3 py-3.5">
      <Skeleton className="size-9 rounded-md" />
      <div className="flex-1 space-y-2">
        <Skeleton className="h-3.5 w-32" />
        <Skeleton className="h-3 w-56 max-w-full" />
      </div>
      <Skeleton className="hidden h-8 w-36 sm:block" />
    </li>
  );
}

/**
 * The signed-in person's invitations, each with Accept and Decline. Nobody
 * joins an organization any other way: accepting goes straight into it.
 */
export function InvitationList({
  onJoined,
  empty,
  className,
}: {
  onJoined?: () => void;
  /** Shown when there are none; nothing by default. */
  empty?: React.ReactNode;
  className?: string;
}) {
  const invitations = useInvitations();
  if (invitations.isPending) {
    return (
      <ul className={cn("divide-y", className)} aria-busy aria-label="Loading invitations">
        <RowSkeleton />
        <RowSkeleton />
      </ul>
    );
  }
  if (invitations.isError && isApiError(invitations.error, "email_unverified")) {
    // Retrying can't help: the sign-in has to change. The API says how, for this person's case.
    return (
      <div role="alert" className={cn("flex gap-3 rounded-xl border bg-muted/40 p-4 text-sm", className)}>
        <MailWarningIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
        <div className="min-w-0">
          <p className="font-medium">Your sign-in can't answer invitations</p>
          <p className="mt-1 break-words text-muted-foreground">{invitations.error.message}</p>
        </div>
      </div>
    );
  }
  if (invitations.isError) {
    return (
      <QueryError
        error={invitations.error}
        onRetry={() => invitations.refetch()}
        what="your invitations"
        className={className}
      />
    );
  }
  if (invitations.data.length === 0) return empty ? <>{empty}</> : null;
  return (
    <ul className={cn("divide-y", className)} aria-label="Invitations">
      {invitations.data.map((invitation) => (
        <InvitationRow key={invitation.id} invitation={invitation} onJoined={onJoined} />
      ))}
    </ul>
  );
}

export function InvitationsDialog({
  open,
  onOpenChange,
  onJoined,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** After joining an organization, as the Console goes there. */
  onJoined?: () => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Invitations</DialogTitle>
          <DialogDescription>Accept one to join that organization. Nobody adds you without it.</DialogDescription>
        </DialogHeader>
        <InvitationList
          onJoined={() => {
            onOpenChange(false);
            onJoined?.();
          }}
          className="-my-2"
          empty={
            <div className="flex flex-col items-center gap-2 py-8 text-center text-sm text-muted-foreground">
              <MailIcon className="size-5" />
              No invitations right now.
            </div>
          }
        />
      </DialogContent>
    </Dialog>
  );
}
