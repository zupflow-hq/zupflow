import type { LocationRewrite } from "@tanstack/react-router";

/**
 * Sandbox lives in the path, like a payment dashboard's test mode:
 *
 *   Production   /northwind/workspaces
 *   Sandbox      /northwind/sandbox/workspaces
 *
 * Routes don't know about the prefix. On the way in, `/{org}/sandbox/…`
 * becomes `/{org}/…?env=sandbox`; on the way out, the reverse. The
 * organization route keeps `env` across navigations, so every link stays in
 * its environment.
 */
const SEGMENT = "sandbox";

/** First path segments that aren't organizations. Keep in step with reserved slugs. */
const NOT_ORGANIZATIONS = new Set([
  "",
  "sign-in",
  "sign-up",
  "onboarding",
  "connect",
  "api",
  "settings",
  "new",
  "help",
]);

function split(pathname: string) {
  const [, first = "", ...rest] = pathname.split("/");
  return { first, rest };
}

export const environmentRewrite: LocationRewrite = {
  input: ({ url }) => {
    const { first, rest } = split(url.pathname);
    if (NOT_ORGANIZATIONS.has(first) || rest[0] !== SEGMENT) return undefined;
    const next = new URL(url);
    next.pathname = `/${[first, ...rest.slice(1)].join("/")}`.replace(/\/$/, "") || "/";
    next.searchParams.set("env", "sandbox");
    return next;
  },
  output: ({ url }) => {
    const env = url.searchParams.get("env");
    if (!env) return undefined;
    const next = new URL(url);
    next.searchParams.delete("env");
    const { first, rest } = split(url.pathname);
    if (env === "sandbox" && !NOT_ORGANIZATIONS.has(first)) {
      next.pathname = `/${[first, SEGMENT, ...rest].join("/")}`.replace(/\/$/, "");
    }
    return next;
  },
};

/**
 * Older links carried the environment as `?env=`. Rewrite the address once at
 * startup so the bar always shows the path form; in-app links already do.
 */
export function canonicalizeEnvironmentUrl() {
  const url = new URL(window.location.href);
  if (!url.searchParams.has("env")) return;
  const next = environmentRewrite.output?.({ url });
  if (next) window.history.replaceState(window.history.state, "", new URL(next).toString());
}
