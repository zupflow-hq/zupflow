import type { CreatedWebhookEndpoint, Organization, WebhookEndpoint } from "@zupflow/sdk";
import { EllipsisIcon, PencilIcon, RefreshCwIcon, SendIcon, Trash2Icon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { SecretRevealDialog } from "@/components/app/secret-reveal";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { canAdminister } from "@/features/context";
import { useRollWebhookSecret, useTestWebhook } from "@/features/queries";
import { isApiError, toastError } from "@/lib/errors";
import { fullDateTime } from "@/lib/format";

type Props = {
  org: Organization;
  endpoint: WebhookEndpoint;
  onEdit: () => void;
  onDelete: () => void;
};

export function WebhookSecretOverlap({ endpoint }: { endpoint: WebhookEndpoint }) {
  const until = endpoint.previous_signing_secret_expires_at;
  return until ? (
    <p className="text-xs text-muted-foreground">
      Deliveries also use the previous secret until <time dateTime={until}>{fullDateTime(until)}</time>. You can rotate
      again after that.
    </p>
  ) : null;
}

export function WebhookEndpointActions(props: Props) {
  if (!canAdminister(props.org)) return null;
  // Changing scope or losing permission discards any secret still on screen.
  return <EndpointActions key={`${props.org.id}:${props.endpoint.environment}:${props.endpoint.id}`} {...props} />;
}

function EndpointActions({ org, endpoint, onEdit, onDelete }: Props) {
  const [confirming, setConfirming] = useState(false);
  const [revealed, setRevealed] = useState<CreatedWebhookEndpoint | null>(null);
  const test = useTestWebhook(org.id);
  const roll = useRollWebhookSecret(org.id, (created) => {
    setConfirming(false);
    setRevealed(created);
    toast.success("Signing secret rotated");
  });
  const until = revealed?.endpoint.previous_signing_secret_expires_at;

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="icon-sm" aria-label="Endpoint actions">
            <EllipsisIcon />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem
            disabled={test.isPending}
            onSelect={() =>
              test.mutate(endpoint.id, {
                onSuccess: (delivery) =>
                  delivery.status_code >= 200 && delivery.status_code < 300
                    ? toast.success(`Test event delivered (${delivery.status_code})`)
                    : toast.error(
                        delivery.status_code
                          ? `Your endpoint answered ${delivery.status_code}`
                          : "No response from your endpoint",
                        { description: "Open delivery history for the result and failure details." },
                      ),
                onError: (error) => toastError(error),
              })
            }
          >
            <SendIcon />
            Send test event
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={onEdit}>
            <PencilIcon />
            Edit
          </DropdownMenuItem>
          <DropdownMenuItem
            disabled={roll.isPending || endpoint.previous_signing_secret_expires_at !== null}
            onSelect={() => setConfirming(true)}
          >
            <RefreshCwIcon />
            Rotate signing secret
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem variant="destructive" onSelect={onDelete}>
            <Trash2Icon />
            Delete
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <ConfirmDialog
        open={confirming}
        onOpenChange={(open) => !roll.isPending && setConfirming(open)}
        title="Rotate this signing secret?"
        description={
          <>
            <p>
              <span className="break-all">{endpoint.url}</span> · {ENVIRONMENT_LABEL[endpoint.environment]}
            </p>
            <p>
              Deliveries will carry signatures from both secrets for 24 hours. Update your receiver before the previous
              secret expires. You cannot rotate again during that overlap.
            </p>
            <p>The new secret will be shown once. Store it before closing the next dialog.</p>
          </>
        }
        actionLabel="Rotate secret"
        destructive={false}
        pending={roll.isPending}
        onConfirm={() =>
          roll.mutate(endpoint.id, {
            onError: (error) => {
              if (isApiError(error, "rotation_in_progress", "idempotency_key_completed")) setConfirming(false);
              toastError(error);
            },
          })
        }
      />
      <SecretRevealDialog
        title="New signing secret"
        description={
          <>
            Update your receiver to verify signatures with this secret. The previous secret stops signing deliveries
            {until ? ` on ${fullDateTime(until)}` : " after 24 hours"}.
          </>
        }
        secret={revealed?.signing_secret ?? null}
        onClose={() => {
          setRevealed(null);
          roll.reset();
        }}
      />
    </>
  );
}
