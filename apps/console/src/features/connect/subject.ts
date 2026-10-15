import { CHANNEL_PROVIDER_NAMES, type ConnectSession } from "@zupflow/sdk";

/** What a Connect session connects, by name: its app, or for a channel session its provider. */
export function connectSubject(session: Pick<ConnectSession, "app" | "target">): string {
  if (session.app) return session.app.name;
  return session.target ? CHANNEL_PROVIDER_NAMES[session.target.provider] : "the app";
}
