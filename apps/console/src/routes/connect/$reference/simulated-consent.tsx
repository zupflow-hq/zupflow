import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { CHANNEL_PROVIDER_NAMES, type ChannelProvider } from "@zupflow/sdk";
import { FlaskConicalIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { AppIcon } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { CHANNEL_PROVIDER_ICON } from "@/features/channels/labels";
import { ConnectFrame } from "@/features/connect/frame";
import { connectSubject } from "@/features/connect/subject";
import { useHostedConnectSession } from "@/features/queries";
import { config } from "@/lib/env";
import { errorMessage } from "@/lib/errors";
import { qk } from "@/lib/query-keys";

export const Route = createFileRoute("/connect/$reference/simulated-consent")({
  component: SimulatedConsentPage,
});

/**
 * Stands in for a provider's OAuth consent screen while the Console runs on
 * the mock API. With a real backend, Connect sends the browser to the
 * provider instead and this page is never reached.
 */
function SimulatedConsentPage() {
  const { reference } = Route.useParams();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const session = useHostedConnectSession(reference);
  const [email, setEmail] = useState("");

  useEffect(() => {
    const expected = session.data?.expected_account?.email;
    if (expected) setEmail(expected);
  }, [session.data?.expected_account?.email]);

  const decide = useMutation({
    mutationFn: async (decision: "approved" | "denied") => {
      // Mock-only endpoint, deliberately outside the SDK: a real provider redirects instead.
      const response = await fetch(
        `${config.apiUrl}/v1/connect/sessions/${encodeURIComponent(reference)}/simulate-consent`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ decision, email: decision === "approved" ? email : null }),
        },
      );
      return response.ok;
    },
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: qk.hostedConnect(reference) });
      await navigate({ to: "/connect/$reference", params: { reference } });
    },
  });

  if (!config.mockApi) {
    return (
      <ConnectFrame>
        <p className="text-center text-sm text-muted-foreground">This page only exists with the mock API.</p>
      </ConnectFrame>
    );
  }

  return (
    <ConnectFrame>
      <div className="-mx-6 -mt-6 mb-6 flex items-center gap-2 rounded-t-2xl border-b border-dashed bg-muted/50 px-6 py-2.5 text-xs text-muted-foreground sm:-mx-8 sm:-mt-8 sm:px-8">
        <FlaskConicalIcon className="size-3.5" />
        Simulated provider consent — mock API
      </div>
      {session.data?.target ? (
        <ChannelConsent
          label={session.data.label}
          provider={session.data.target.provider}
          pending={decide.isPending}
          error={decide.isError ? errorMessage(decide.error) : null}
          onDecide={(decision) => decide.mutate(decision)}
        />
      ) : session.data ? (
        <form
          className="space-y-5"
          onSubmit={(e) => {
            e.preventDefault();
            decide.mutate("approved");
          }}
        >
          <div className="flex items-center gap-3">
            <AppIcon icon={session.data.app?.icon ?? { glyph: "#", color: "#64748b" }} size="lg" />
            <div>
              <h1 className="font-semibold">Sign in to {connectSubject(session.data)}</h1>
              <p className="text-sm text-muted-foreground">to continue to the requesting app</p>
            </div>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="consent-email">Account</Label>
            <Input
              id="consent-email"
              type="email"
              required
              placeholder="you@example.com"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">
              The provider identifies the account by this address. Reconnecting with another one is refused.
            </p>
          </div>
          {session.data.requested_scopes.length > 0 ? (
            <div className="space-y-1.5 text-sm">
              <p className="font-medium">This will allow access to</p>
              <ul className="list-disc space-y-0.5 pl-5 text-muted-foreground">
                {session.data.requested_scopes.map((s) => (
                  <li key={s} className="break-all">
                    {s}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
          {decide.isError ? <p className="text-sm text-destructive">{errorMessage(decide.error)}</p> : null}
          <div className="flex gap-2">
            <Button
              type="button"
              variant="outline"
              className="flex-1"
              disabled={decide.isPending}
              onClick={() => decide.mutate("denied")}
            >
              Deny
            </Button>
            <Button type="submit" className="flex-1" disabled={decide.isPending || !email}>
              {decide.isPending ? <Spinner /> : null}
              Allow
            </Button>
          </div>
        </form>
      ) : (
        <div className="space-y-4">
          <Skeleton className="h-12 w-48" />
          <Skeleton className="h-9 w-full" />
          <Skeleton className="h-9 w-full" />
        </div>
      )}
    </ConnectFrame>
  );
}

/** A channel app's Allow page: the team admin adds the agent's app, no account to name. */
function ChannelConsent({
  label,
  provider,
  pending,
  error,
  onDecide,
}: {
  label: string;
  provider: ChannelProvider;
  pending: boolean;
  error: string | null;
  onDecide: (decision: "approved" | "denied") => void;
}) {
  const name = CHANNEL_PROVIDER_NAMES[provider];
  return (
    <div className="space-y-5">
      <div className="flex items-center gap-3">
        <AppIcon icon={CHANNEL_PROVIDER_ICON[provider]} size="lg" />
        <div className="min-w-0">
          <h1 className="font-semibold">
            {label} is requesting permission to access your {name} team
          </h1>
        </div>
      </div>
      <div className="space-y-1.5 text-sm">
        <p className="font-medium">It will be able to</p>
        <ul className="list-disc space-y-0.5 pl-5 text-muted-foreground">
          <li>View messages that mention it and direct messages sent to it</li>
          <li>View messages in conversations it's added to</li>
          <li>Send messages and add reactions as {label}</li>
        </ul>
      </div>
      {error ? <p className="text-sm text-destructive">{error}</p> : null}
      <div className="flex gap-2">
        <Button
          type="button"
          variant="outline"
          className="flex-1"
          disabled={pending}
          onClick={() => onDecide("denied")}
        >
          Cancel
        </Button>
        <Button type="button" className="flex-1" disabled={pending} onClick={() => onDecide("approved")}>
          {pending ? <Spinner /> : null}
          Allow
        </Button>
      </div>
    </div>
  );
}
