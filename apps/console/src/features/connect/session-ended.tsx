import type { ConnectSession } from "@zupflow/sdk";
import { ArrowLeftIcon, ClockIcon, XCircleIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { hostedFailure } from "./failure";
import { ConnectOutcome } from "./frame";

/** Where the person goes back to: the session's return URL, told which session ended. */
export function returnUrlFor(session: Pick<ConnectSession, "id" | "return_url">): string | null {
  if (!session.return_url) return null;
  const url = new URL(session.return_url);
  url.searchParams.set("zupflow_session", session.id);
  return url.toString();
}

/**
 * Hosted Connect after a session ended without connecting: what happened and
 * what the person can do. Nothing here starts the session again: a failed,
 * expired or cancelled session is final, and only the product that sent the
 * person here can start another.
 */
export function SessionEnded({
  session,
  canClose = false,
}: {
  session: Pick<ConnectSession, "id" | "app" | "target" | "status" | "failure_code" | "return_url">;
  /** Whether this window was opened by another, so closing it returns the person there. */
  canClose?: boolean;
}) {
  const failure = session.status === "failed" ? hostedFailure(session) : null;
  const [title, body] =
    session.status === "expired"
      ? ["This link expired", "Connect links last 15 minutes. Nothing was connected. Start again from the app."]
      : session.status === "cancelled"
        ? ["Connect was cancelled", "Nothing was connected."]
        : [failure?.title ?? "Couldn't connect", failure?.body ?? "Nothing was connected."];
  const back = returnUrlFor(session);
  return (
    <ConnectOutcome
      icon={
        session.status === "expired" ? (
          <ClockIcon className="size-9 text-muted-foreground" />
        ) : (
          <XCircleIcon className="size-9 text-destructive" />
        )
      }
      title={title}
      body={body}
    >
      {back ? (
        <Button asChild variant="outline" className="mt-2 w-full">
          <a href={back}>
            <ArrowLeftIcon />
            Go back
          </a>
        </Button>
      ) : canClose ? (
        <Button variant="outline" className="mt-2 w-full" onClick={() => window.close()}>
          Close window
        </Button>
      ) : null}
    </ConnectOutcome>
  );
}
