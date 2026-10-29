import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  CHANNEL_PROVIDER_NAMES,
  type ChannelAppManagerConnectResult,
  type ConnectSession,
  type ConnectTarget,
  ZupflowApiError,
} from "@zupflow/sdk";
import { AlertCircleIcon, ArrowLeftIcon, CheckCircle2Icon, ExternalLinkIcon, RotateCwIcon } from "lucide-react";
import { useState } from "react";
import { EnvBadge } from "@/components/app/environment";
import { AppIcon } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import {
  EMPTY_TOKENS,
  missingTokens,
  SlackTokenSteps,
  TokenFields,
  type TokenValues,
  tokenFieldErrors,
} from "@/features/channels/configuration-token";
import { CHANNEL_PROVIDER_ICON } from "@/features/channels/labels";
import { api } from "@/lib/api";
import { errorMessage } from "@/lib/errors";
import { qk } from "@/lib/query-keys";
import { hostedFailure } from "./failure";
import { useAfterSession } from "./finish";
import { ConnectOutcome } from "./frame";
import { returnUrlFor, SessionEnded } from "./session-ended";

/** A channel session: it has a target instead of an app. */
export type ChannelConnectSession = ConnectSession & { target: ConnectTarget };

/**
 * Hosted Connect for a channel: linking a Slack team by pasting a
 * configuration token, or installing one agent's app by allowing it in Slack.
 * The person here is the customer's team admin, not the developer.
 */
export function ChannelConnectBody({ session, reference }: { session: ChannelConnectSession; reference: string }) {
  const queryClient = useQueryClient();
  const refresh = () => queryClient.invalidateQueries({ queryKey: qk.hostedConnect(reference) });
  const cancel = useMutation({ mutationFn: () => api.connect.cancel(reference), onSettled: refresh });
  useAfterSession(session);
  const provider = CHANNEL_PROVIDER_NAMES[session.target.provider];
  const linking = session.target.kind === "channel_app_manager";

  if (session.status === "completed") {
    return (
      <ConnectOutcome
        icon={<CheckCircle2Icon className="size-9 text-success" />}
        title={linking ? `${provider} team linked` : `${session.label} added to ${provider}`}
        body={
          <>
            {linking
              ? "Agents can now be added to your team, each as its own app you allow."
              : `People in your team can mention ${session.label} and send it messages.`}{" "}
            {session.return_url ? "Taking you back…" : "You can close this window."}
          </>
        }
      />
    );
  }
  if (session.status !== "pending") {
    return <SessionEnded session={session} canClose={Boolean(window.opener)} />;
  }
  if (session.unavailable_reason) {
    return <LinkUnavailable session={session} />;
  }

  return (
    <div>
      <div className="flex flex-col items-center gap-3 text-center">
        <AppIcon icon={CHANNEL_PROVIDER_ICON[session.target.provider]} size="lg" />
        <div className="space-y-1">
          <h1 className="text-lg font-semibold">
            {linking ? `Link your ${provider} team` : `Add ${session.label} to ${provider}`}
          </h1>
          <p className="text-sm text-pretty text-muted-foreground">
            {linking
              ? `So agents can be added to your ${provider} team. Each one gets its own ${provider} app, which you allow separately.`
              : `${session.label} gets its own app in your ${provider} team. ${provider} asks you to allow it.`}
          </p>
        </div>
        {session.environment === "sandbox" ? <EnvBadge environment="sandbox" size="sm" /> : null}
      </div>
      <div className="mt-7">
        {linking ? (
          <LinkTeamForm session={session} reference={reference} onDone={refresh} />
        ) : (
          <AllowApp session={session} reference={reference} onFailed={refresh} />
        )}
      </div>
      <Button
        variant="ghost"
        className="mt-2 w-full text-muted-foreground"
        disabled={cancel.isPending}
        onClick={() => cancel.mutate()}
      >
        Cancel
      </Button>
    </div>
  );
}

/**
 * A link that can't link its team right now (its provider isn't allowed, or
 * isn't reached), shown instead of asking for tokens the person would make
 * for nothing. They're the customer's team admin and can change neither, so
 * they're told only what to do. The session stays pending.
 */
function LinkUnavailable({ session }: { session: ChannelConnectSession }) {
  const back = returnUrlFor(session);
  return (
    <ConnectOutcome
      icon={<AlertCircleIcon className="size-9 text-muted-foreground" />}
      title="This link can't be used right now"
      body="Nothing was linked. Ask the app that sent you here for a new one."
    >
      {back ? (
        <Button asChild variant="outline" className="mt-2 w-full">
          <a href={back}>
            <ArrowLeftIcon />
            Go back
          </a>
        </Button>
      ) : window.opener ? (
        <Button variant="outline" className="mt-2 w-full" onClick={() => window.close()}>
          Close window
        </Button>
      ) : null}
    </ConnectOutcome>
  );
}

/** Pasting the team's configuration token: checked with the provider before anything is kept. */
function LinkTeamForm({
  session,
  reference,
  onDone,
}: {
  session: ChannelConnectSession;
  reference: string;
  onDone: () => void;
}) {
  const [values, setValues] = useState<TokenValues>(EMPTY_TOKENS);
  const [missing, setMissing] = useState<Partial<Record<keyof TokenValues, string>>>({});
  const submit = useMutation({
    gcTime: 0,
    mutationFn: (tokens: TokenValues) =>
      api.connect.submitCredentials<ChannelAppManagerConnectResult>(reference, { values: { ...tokens } }),
    onSuccess: () => {
      setValues(EMPTY_TOKENS);
      onDone();
    },
    // A session that ended meanwhile shows why; a pending one keeps the form and its values.
    onError: () => onDone(),
  });
  const server = tokenFieldErrors(submit.error);
  const provider = CHANNEL_PROVIDER_NAMES[session.target.provider];
  return (
    <form
      className="space-y-5"
      autoComplete="off"
      onSubmit={(e) => {
        e.preventDefault();
        const absent = missingTokens(values);
        setMissing(absent);
        if (Object.keys(absent).length === 0) submit.mutate(values);
      }}
    >
      {session.target.provider === "slack" ? <SlackTokenSteps /> : null}
      <TokenFields
        idPrefix="connect"
        values={values}
        onChange={(next) => {
          setValues(next);
          setMissing({});
        }}
        errors={{ ...server, ...missing }}
        disabled={submit.isPending}
      />
      {submit.isError && Object.keys(server).length === 0 ? (
        <p role="alert" className="text-sm text-destructive">
          {errorMessage(submit.error)}
        </p>
      ) : null}
      <Button type="submit" className="h-10 w-full" disabled={submit.isPending}>
        {submit.isPending ? <Spinner /> : null}
        Link team
      </Button>
      <p className="text-center text-xs text-muted-foreground">
        Sent straight to Zupflow and encrypted. Used only to create and manage these agents' apps in {provider}.
      </p>
    </form>
  );
}

/** Installing one agent's app: the provider's Allow page, then back here. */
function AllowApp({
  session,
  reference,
  onFailed,
}: {
  session: ChannelConnectSession;
  reference: string;
  onFailed: () => Promise<void>;
}) {
  const authorize = useMutation({
    mutationFn: () => api.connect.authorize(reference),
    onSuccess: ({ authorization_url }) => window.location.assign(authorization_url),
    onError: () => onFailed(),
  });
  const retry = authorize.isError && (!(authorize.error instanceof ZupflowApiError) || authorize.error.status >= 500);
  const provider = CHANNEL_PROVIDER_NAMES[session.target.provider];
  return (
    <div className="space-y-4">
      <div className="rounded-lg border bg-muted/30 p-3">
        <p className="mb-2 text-xs font-medium text-muted-foreground">Once allowed, {session.label} can</p>
        <ul className="list-disc space-y-1 pl-4 text-sm">
          <li>Answer when someone mentions it or sends it a message</li>
          <li>Read messages in conversations it's added to</li>
          <li>Post and react as {session.label}</li>
        </ul>
      </div>
      {authorize.isError ? (
        <p role="alert" className="text-sm text-destructive">
          {retry
            ? errorMessage(authorize.error)
            : hostedFailure({
                app: null,
                target: session.target,
                label: session.label,
                failure_code: authorize.error instanceof ZupflowApiError ? authorize.error.code : null,
              }).body}
        </p>
      ) : null}
      <Button
        className="h-10 w-full"
        disabled={authorize.isPending || (authorize.isError && !retry)}
        onClick={() => authorize.mutate()}
      >
        {authorize.isPending ? <Spinner /> : retry ? <RotateCwIcon /> : <ExternalLinkIcon />}
        {retry ? "Try again" : `Continue to ${provider}`}
      </Button>
      <p className="text-center text-xs text-muted-foreground">
        You'll allow it on {provider}, signed in as someone who may install apps in your team. Zupflow never sees your
        password.
      </p>
    </div>
  );
}
