import type { ConnectSession } from "@zupflow/sdk";
import { useEffect } from "react";
import { returnUrlFor } from "./session-ended";

function notifyOpener(sessionId: string) {
  try {
    window.opener?.postMessage({ type: "zupflow.connect.finished", session_id: sessionId }, window.location.origin);
  } catch {
    // The opener is gone or on another origin; the embedding backend still verifies via the API.
  }
}

/**
 * Once a hosted session leaves `pending`: tells the window that opened this
 * one to check, and after a completed one goes back to the return URL, or
 * closes a popup.
 */
export function useAfterSession(session: Pick<ConnectSession, "id" | "status" | "return_url">) {
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
}
