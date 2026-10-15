import type { ConnectSession } from "@zupflow/sdk";
import { connectSubject } from "./subject";

/**
 * What the person on hosted Connect can do after a failure: start again from
 * the product that sent them (it may work next time), or contact that
 * product's support (trying again fails the same way).
 */
export type NextStep = "start_again" | "contact";

export interface HostedFailure {
  title: string;
  body: string;
  next: NextStep;
}

const FROM = "the app that sent you here";

/**
 * Why a hosted Connect session failed, written for the person connecting
 * their account: they aren't the developer, can't change the app's setup,
 * and only need to know whether starting again can help.
 */
export function hostedFailure(
  session: Pick<ConnectSession, "app" | "target" | "failure_code"> & Partial<Pick<ConnectSession, "label">>,
): HostedFailure {
  if (session.target) return channelFailure(session.target, session.label ?? null, session.failure_code);
  const app = connectSubject(session);
  const mcp = session.app?.auth_type === "mcp_oauth";
  switch (session.failure_code) {
    case "consent_denied":
      return {
        title: "Access wasn't granted",
        body: `Nothing was connected. To connect ${app}, start again from ${FROM} and allow access.`,
        next: "start_again",
      };
    case "account_mismatch":
      return {
        title: "That's a different account",
        body: `Nothing changed. Start again from ${FROM} and sign in with the ${app} account that was connected before.`,
        next: "start_again",
      };
    case "discovery_failed":
      return {
        title: `${app} can't be connected yet`,
        body: `${app}'s server doesn't offer a sign-in Zupflow can use, so nothing was connected. Starting again won't help: contact support for ${FROM} and tell them ${app} couldn't be connected.`,
        next: "contact",
      };
    case "identity_off_server":
      return {
        title: `${app} isn't set up correctly`,
        body: `Zupflow stopped before sending anything to ${app}, so nothing was connected. Starting again won't help: contact support for ${FROM} and tell them ${app} couldn't be connected.`,
        next: "contact",
      };
    case "provider_unavailable":
      return {
        title: `${app} couldn't be reached`,
        body: `Nothing was connected. This is usually brief: wait a few minutes, then start again from ${FROM}.`,
        next: "start_again",
      };
    case "token_exchange_failed":
      return {
        title: `${app} didn't finish signing you in`,
        body: `${mcp ? `${app}'s server` : app} didn't accept the sign-in, so nothing was connected. Start again from ${FROM}. If it keeps happening, contact its support.`,
        next: "start_again",
      };
    case "authorization_failed":
      return {
        title: `${app} ended the sign-in`,
        body: `${app} stopped the sign-in with an error, so nothing was connected. Start again from ${FROM}.`,
        next: "start_again",
      };
    case "identity_unavailable":
      return {
        title: "Couldn't confirm your account",
        body: `${app} didn't say which account signed in, so nothing was connected. Wait a few minutes, then start again from ${FROM}.`,
        next: "start_again",
      };
    case "configuration_not_ready":
    case "installation_changed":
      return {
        title: "Something changed while you were signing in",
        body: `Nothing was connected. Start again from ${FROM}.`,
        next: "start_again",
      };
    case "app_not_eligible":
      return {
        title: `${app} isn't available`,
        body: `${app} can't be connected here anymore, so nothing was connected. If you still need it, contact support for ${FROM}.`,
        next: "contact",
      };
    default:
      return {
        title: "Couldn't connect",
        body: `Nothing was connected. Start again from ${FROM}.`,
        next: "start_again",
      };
  }
}

/**
 * A channel session's failure. Linking a team changes nothing until the
 * tokens are accepted; adding an agent leaves it out of the team until its app
 * is allowed.
 */
function channelFailure(
  target: NonNullable<ConnectSession["target"]>,
  label: string | null,
  code: string | null,
): HostedFailure {
  const provider = connectSubject({ app: null, target });
  const linking = target.kind === "channel_app_manager";
  const what = linking ? `your ${provider} team` : (label ?? "the agent");
  const nothing = linking ? "No team was linked." : `${what} wasn't added to ${provider}.`;
  switch (code) {
    case "consent_denied":
      return {
        title: "The app wasn't allowed",
        body: `${nothing} To add it, start again from ${FROM} and choose Allow on ${provider}.`,
        next: "start_again",
      };
    case "app_not_eligible":
    case "channel_app_not_ready":
      return {
        title: `${what} can't be added right now`,
        body: `Its ${provider} app was removed, or its team's link needs new tokens, so nothing was added. Contact support for ${FROM}.`,
        next: "contact",
      };
    case "provider_unavailable":
      return {
        title: `${provider} couldn't be reached`,
        body: `${nothing} This is usually brief: wait a few minutes, then start again from ${FROM}.`,
        next: "start_again",
      };
    case "authorization_failed":
    case "token_exchange_failed":
      return {
        title: `${provider} didn't finish the install`,
        body: `${nothing} Start again from ${FROM}. If it keeps happening, contact its support.`,
        next: "start_again",
      };
    default:
      return {
        title: linking ? "Couldn't link your team" : `Couldn't add ${what}`,
        body: `${nothing} Start again from ${FROM}.`,
        next: "start_again",
      };
  }
}
