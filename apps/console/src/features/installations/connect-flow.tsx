import { useQueryClient } from "@tanstack/react-query";
import type { ConnectSession, ConnectSessionId, CreateConnectSessionInput, WorkspaceId } from "@zupflow/sdk";
import { CheckCircle2Icon, ExternalLinkIcon, XCircleIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { connectSubject } from "@/features/connect/subject";
import { useCreateConnectSession, useWatchedConnectSession } from "@/features/queries";
import { errorMessage } from "@/lib/errors";
import { qk } from "@/lib/query-keys";

const FAILURE_COPY: Record<string, string> = {
  consent_denied: "Access wasn't granted on the provider's screen. Nothing was connected.",
  account_mismatch:
    "A different account was used. Reconnect with the original account, or add a new installation for the other one.",
  credential_rejected: "The provider rejected the credential.",
  authorization_failed: "The provider ended the sign-in with an error. Start again.",
  token_exchange_failed:
    "The provider didn't accept the sign-in. Start again, or check the app's client ID and secret.",
  discovery_failed:
    "This app's server doesn't offer a sign-in Zupflow can use. Check the MCP server URL in the app's configuration, or ask the server's owner to support OAuth sign-in with client registration.",
  identity_off_server:
    "This app's identity URL isn't on its MCP server. Set it to an address on the server, or clear it, in the app's authentication settings.",
  provider_unavailable: "The provider couldn't be reached. Start again shortly.",
  identity_unavailable: "The provider didn't say which account signed in. Start again shortly.",
  app_not_eligible: "This app is no longer available to this workspace.",
  configuration_not_ready: "This app's configuration changed while you were signing in. Start again.",
  installation_changed: "This installation changed while you were connecting. Start again from the app.",
  unavailable: "Something went wrong on our side. Start again shortly.",
};

/** Why a session failed, in words that fit how its app signs in. */
function failureCopy(session: ConnectSession, code: string): string {
  if (code === "token_exchange_failed" && session.app?.auth_type === "mcp_oauth") {
    return "The app's server didn't accept the sign-in. Start again; if it keeps failing, ask the server's owner to check its sign-in.";
  }
  return FAILURE_COPY[code] ?? `The provider returned ${code}.`;
}

/**
 * Runs Connect in a popup and watches the session server-side. A browser
 * message from the popup only triggers a re-check; completion is believed
 * only when the API says the session completed.
 */
export function useConnectFlow(workspaceId: WorkspaceId) {
  const queryClient = useQueryClient();
  const create = useCreateConnectSession(workspaceId);
  const [sessionId, setSessionId] = useState<ConnectSessionId | null>(null);
  const [hostedUrl, setHostedUrl] = useState<string | null>(null);
  const [blocked, setBlocked] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const popup = useRef<Window | null>(null);
  const session = useWatchedConnectSession(workspaceId, sessionId);
  const { refetch } = session;

  // The hosted page lives at the session's URL, which with a backend is the
  // server's public origin, not the Console's. It posts only to its
  // environment's allowed origins, so a Console not among them never hears
  // it: polling the session (`useWatchedConnectSession`) is what finds the
  // outcome, and a message only makes that happen sooner.
  const hostedOrigin = useMemo(() => {
    try {
      return hostedUrl ? new URL(hostedUrl).origin : null;
    } catch {
      return null;
    }
  }, [hostedUrl]);

  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (hostedOrigin === null || event.origin !== hostedOrigin) return;
      const data = event.data as { type?: string; session_id?: string } | null;
      if (data?.type === "zupflow.connect.finished" && data.session_id === sessionId) {
        void refetch();
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [sessionId, refetch, hostedOrigin]);

  const status = session.data?.status;
  useEffect(() => {
    if (status && status !== "pending") {
      void queryClient.invalidateQueries({ queryKey: qk.workspace(workspaceId) });
    }
  }, [status, queryClient, workspaceId]);

  const start = useCallback(
    (input: CreateConnectSessionInput) => {
      setError(null);
      setBlocked(false);
      // Open synchronously inside the click so the browser allows the popup.
      const win = window.open("", "zupflow-connect", "popup,width=480,height=720");
      popup.current = win;
      create.mutate(input, {
        onSuccess: (s) => {
          setSessionId(s.id);
          setHostedUrl(s.url);
          if (win && !win.closed) win.location.href = s.url;
          else setBlocked(true);
        },
        onError: (e) => {
          win?.close();
          setError(e);
        },
      });
    },
    [create],
  );

  const reopen = () => {
    if (!hostedUrl) return;
    popup.current = window.open(hostedUrl, "zupflow-connect", "popup,width=480,height=720");
    setBlocked(!popup.current);
  };

  const reset = useCallback(() => {
    setSessionId(null);
    setHostedUrl(null);
    setError(null);
    setBlocked(false);
  }, []);

  return {
    start,
    reopen,
    reset,
    starting: create.isPending,
    error,
    blocked,
    sessionId,
    session: session.data,
  };
}

/** What the person sees in the Console while Connect runs in its own window. */
export function ConnectProgress({
  flow,
  onDone,
  doneLabel = "Done",
}: {
  flow: ReturnType<typeof useConnectFlow>;
  onDone: (session: ConnectSession) => void;
  doneLabel?: string;
}) {
  const s = flow.session;
  if (!s || s.status === "pending") {
    return (
      <div className="flex flex-col items-center gap-4 py-8 text-center">
        <Spinner className="size-6 text-brand-ink" />
        <div className="space-y-1">
          <p className="font-medium">Waiting for authorization</p>
          <p className="max-w-sm text-sm text-muted-foreground">
            Finish connecting in the Connect window. This updates when the API confirms the result.
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={flow.reopen} disabled={!flow.sessionId}>
          <ExternalLinkIcon />
          {flow.blocked ? "Open Connect" : "Reopen Connect window"}
        </Button>
        {flow.blocked ? (
          <p className="text-xs text-muted-foreground">
            Your browser blocked the popup. Open it with the button above.
          </p>
        ) : null}
      </div>
    );
  }
  if (s.status === "completed") {
    return (
      <div className="flex flex-col items-center gap-4 py-8 text-center">
        <CheckCircle2Icon className="size-8 text-success" />
        <div className="space-y-1">
          <p className="font-medium">
            {connectSubject(s)} · {s.label} {s.intent === "reconnect" ? "reconnected" : "connected"}
          </p>
          {s.target === null ? (
            <p className="text-sm text-muted-foreground">Agents still need a grant before they can use it.</p>
          ) : null}
        </div>
        <Button onClick={() => onDone(s)}>{doneLabel}</Button>
      </div>
    );
  }
  return (
    <div className="flex flex-col items-center gap-4 py-8 text-center">
      <XCircleIcon className="size-8 text-destructive" />
      <div className="space-y-1">
        <p className="font-medium">
          {s.status === "expired"
            ? "The Connect session expired"
            : s.status === "cancelled"
              ? "Connect was cancelled"
              : "Connect didn't finish"}
        </p>
        <p className="max-w-sm text-sm text-muted-foreground">
          {s.failure_code ? failureCopy(s, s.failure_code) : "Nothing was connected. You can start again."}
        </p>
      </div>
      <Button variant="outline" onClick={flow.reset}>
        Start again
      </Button>
    </div>
  );
}

export function ConnectError({ error }: { error: unknown }) {
  if (!error) return null;
  return (
    <p role="alert" className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
      {errorMessage(error)}
    </p>
  );
}
