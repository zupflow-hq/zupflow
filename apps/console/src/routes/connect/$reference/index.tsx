import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import type { ConnectSession, TokenAuthTemplate } from "@zupflow/sdk";
import { ZupflowApiError } from "@zupflow/sdk";
import { CheckCircle2Icon, ExternalLinkIcon, RotateCwIcon, XCircleIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { EnvBadge } from "@/components/app/environment";
import { AppIcon } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { hostedFailure } from "@/features/connect/failure";
import { ConnectFrame, ConnectOutcome } from "@/features/connect/frame";
import { returnUrlFor, SessionEnded } from "@/features/connect/session-ended";
import { connectSubject } from "@/features/connect/subject";
import { useDocumentTitle } from "@/features/context";
import { useHostedConnectSession } from "@/features/queries";
import { api } from "@/lib/api";
import { errorMessage } from "@/lib/errors";
import { qk } from "@/lib/query-keys";

export const Route = createFileRoute("/connect/$reference/")({
  component: HostedConnectPage,
});

function notifyOpener(sessionId: string) {
  try {
    window.opener?.postMessage({ type: "zupflow.connect.finished", session_id: sessionId }, window.location.origin);
  } catch {
    // The opener is gone or on another origin; the embedding backend still verifies via the API.
  }
}

function HostedConnectPage() {
  // The link's last segment is the session's reference: possession authorizes this page.
  const { reference } = Route.useParams();
  const session = useHostedConnectSession(reference);
  useDocumentTitle(session.data?.app ? `Connect ${session.data.app.name}` : "Connect");

  // A failed re-read keeps the session it already had: it may still be pending.
  if (session.isError && !session.data) {
    const notFound = session.error instanceof ZupflowApiError && session.error.status === 404;
    return (
      <ConnectFrame>
        <ConnectOutcome
          icon={<XCircleIcon className="size-9 text-destructive" />}
          title={notFound ? "This link isn't valid" : "Connect couldn't load"}
          body={notFound ? "Start again from the app that sent you here." : errorMessage(session.error)}
        />
      </ConnectFrame>
    );
  }
  if (!session.data) {
    return (
      <ConnectFrame>
        <div className="flex flex-col items-center gap-3">
          <Skeleton className="size-12 rounded-lg" />
          <Skeleton className="h-6 w-40" />
          <Skeleton className="h-4 w-56" />
        </div>
        <div className="mt-8 space-y-3">
          <Skeleton className="h-9 w-full" />
          <Skeleton className="h-9 w-full" />
        </div>
      </ConnectFrame>
    );
  }
  const { app } = session.data;
  if (app === null) {
    // A channel session: linking a team or installing an agent's app has no page here yet.
    return (
      <ConnectFrame>
        <ConnectOutcome
          icon={<XCircleIcon className="size-9 text-destructive" />}
          title={`${connectSubject(session.data)} can't be set up on this page yet`}
          body="Nothing was changed. Start again from the app that sent you here."
        />
      </ConnectFrame>
    );
  }
  return (
    <ConnectFrame>
      <ConnectBody session={{ ...session.data, app }} reference={reference} />
    </ConnectFrame>
  );
}

/** An app's session: the only kind this page connects so far. */
type AppConnectSession = ConnectSession & { app: NonNullable<ConnectSession["app"]> };

function ConnectBody({ session, reference }: { session: AppConnectSession; reference: string }) {
  const queryClient = useQueryClient();
  const refresh = () => queryClient.invalidateQueries({ queryKey: qk.hostedConnect(reference) });
  const cancel = useMutation({ mutationFn: () => api.connect.cancel(reference), onSettled: refresh });

  const back = returnUrlFor(session);
  useEffect(() => {
    if (session.status === "pending") return;
    notifyOpener(session.id);
    if (session.status === "completed" && back) {
      const t = setTimeout(() => window.location.assign(back), 1200);
      return () => clearTimeout(t);
    }
    if (session.status === "completed" && window.opener) {
      const t = setTimeout(() => window.close(), 1600);
      return () => clearTimeout(t);
    }
  }, [session.status, session.id, back]);

  if (session.status === "completed") {
    return (
      <ConnectOutcome
        icon={<CheckCircle2Icon className="size-9 text-success" />}
        title={`${session.app.name} connected`}
        body={session.return_url ? "Taking you back…" : "You can close this window."}
      />
    );
  }
  if (session.status !== "pending") {
    return <SessionEnded session={session} canClose={Boolean(window.opener)} />;
  }

  return (
    <div>
      <div className="flex flex-col items-center gap-3 text-center">
        <AppIcon icon={session.app.icon} size="lg" />
        <div className="space-y-1">
          <h1 className="text-lg font-semibold">
            {session.intent === "reconnect" ? "Reconnect" : "Connect"} {session.app.name}
          </h1>
          <p className="text-sm text-muted-foreground">
            {!session.expected_account
              ? `So your assistant can work with your ${session.app.name} account.`
              : session.auth?.type === "mcp_oauth" && !session.auth.identity_url
                ? // Nothing names the account, so the person is the only check that it's the same one.
                  `Sign in with the same ${session.app.name} account you connected before. Zupflow may not be able to tell which account it is, so make sure it's the same one.`
                : `Use ${session.expected_account.email ?? session.expected_account.display_name} — the account already linked.`}
          </p>
        </div>
        {session.environment === "sandbox" ? <EnvBadge environment="sandbox" size="sm" /> : null}
      </div>
      <div className="mt-7">
        {session.auth?.type === "token" ? (
          <TokenForm session={session} reference={reference} auth={session.auth} onDone={refresh} />
        ) : (
          <OAuthStart session={session} reference={reference} onFailed={refresh} />
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

function OAuthStart({
  session,
  reference,
  onFailed,
}: {
  session: AppConnectSession;
  reference: string;
  /** Re-reads the session: a start the provider's side refused fails it, and the page shows why. */
  onFailed: () => Promise<void>;
}) {
  const authorize = useMutation({
    mutationFn: () => api.connect.authorize(reference),
    onSuccess: ({ authorization_url }) => window.location.assign(authorization_url),
    // Awaited, so the button stays busy until the page knows whether the session is still pending.
    onError: () => onFailed(),
  });
  // The session is still pending here (a failed one shows why instead). Our side or the network failing can pass;
  // anything the request itself got wrong fails the same way again.
  const retry = authorize.isError && (!(authorize.error instanceof ZupflowApiError) || authorize.error.status >= 500);
  return (
    <div className="space-y-4">
      {session.requested_scopes.length > 0 ? (
        <div className="rounded-lg border bg-muted/30 p-3">
          <p className="mb-2 text-xs font-medium text-muted-foreground">{session.app.name} will ask you to allow</p>
          <ul className="space-y-1">
            {session.requested_scopes.map((s) => (
              <li key={s} className="truncate font-mono text-[11.5px]" title={s}>
                {s.replace("https://www.googleapis.com/auth/", "")}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {authorize.isError ? (
        <p role="alert" className="text-sm text-destructive">
          {retry
            ? errorMessage(authorize.error)
            : // The server's words are for the app's developer; the person here gets what they can do.
              hostedFailure({
                app: session.app,
                target: session.target,
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
        {retry ? "Try again" : `Continue to ${session.app.name}`}
      </Button>
      <p className="text-center text-xs text-muted-foreground">
        You'll sign in on {session.app.name}. Zupflow never sees your password.
      </p>
    </div>
  );
}

function TokenForm({
  session,
  reference,
  auth,
  onDone,
}: {
  session: AppConnectSession;
  reference: string;
  auth: TokenAuthTemplate;
  onDone: () => void;
}) {
  const [values, setValues] = useState<Record<string, string>>({});
  const submit = useMutation({
    mutationFn: () => api.connect.submitCredentials(reference, { values }),
    onSuccess: () => {
      setValues({});
      onDone();
    },
    // A session that ended meanwhile shows why; a pending one keeps the form and its values.
    onError: () => onDone(),
  });
  const fieldErrors = submit.error instanceof ZupflowApiError ? (submit.error.fields ?? {}) : {};

  return (
    <form
      className="space-y-4"
      autoComplete="off"
      onSubmit={(e) => {
        e.preventDefault();
        submit.mutate();
      }}
    >
      <p className="text-sm text-muted-foreground">{auth.instructions}</p>
      {auth.inputs.map((input) => (
        <div key={input.key} className="space-y-1.5">
          <Label htmlFor={`connect-${input.key}`}>{input.label}</Label>
          <Input
            id={`connect-${input.key}`}
            type={input.secret ? "password" : "text"}
            className="font-mono text-sm"
            placeholder={input.placeholder ?? undefined}
            autoComplete="off"
            spellCheck={false}
            aria-invalid={Boolean(fieldErrors[input.key])}
            value={values[input.key] ?? ""}
            onChange={(e) => setValues((v) => ({ ...v, [input.key]: e.target.value }))}
          />
          {fieldErrors[input.key] ? (
            <p className="text-sm text-destructive">{fieldErrors[input.key]}</p>
          ) : input.help ? (
            <p className="text-xs text-muted-foreground">{input.help}</p>
          ) : null}
        </div>
      ))}
      {submit.isError && Object.keys(fieldErrors).length === 0 ? (
        <p className="text-sm text-destructive">{errorMessage(submit.error)}</p>
      ) : null}
      {auth.docs_url ? (
        <a
          href={auth.docs_url}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1 text-sm text-brand-ink underline-offset-4 hover:underline"
        >
          Where do I find this? <ExternalLinkIcon className="size-3" />
        </a>
      ) : null}
      <Button type="submit" className="h-10 w-full" disabled={submit.isPending}>
        {submit.isPending ? <Spinner /> : null}
        {session.intent === "reconnect" ? "Reconnect" : "Connect"}
      </Button>
      <p className="text-center text-xs text-muted-foreground">
        Sent straight to Zupflow and encrypted. {session.app.name} is the only service it's used with.
      </p>
    </form>
  );
}
