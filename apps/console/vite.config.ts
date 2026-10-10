import { rm } from "node:fs/promises";
import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, loadEnv, type Plugin } from "vite";

/**
 * Whether the Console contains the mock API: the dev server does; a build
 * (whatever its `--mode`) only when made with `VITE_ZUPFLOW_MOCK=1`.
 */
function mockIncluded(command: "build" | "serve", flag: string | undefined) {
  return command === "serve" || flag === "1";
}

/** Leaves the mock's service worker (copied from `public/`) out of a build without the mock. */
function withoutMockWorker(): Plugin {
  let outDir = "dist";
  return {
    name: "zupflow:without-mock-worker",
    apply: "build",
    configResolved(resolved) {
      outDir = path.resolve(resolved.root, resolved.build.outDir);
    },
    async closeBundle() {
      await rm(path.join(outDir, "mockServiceWorker.js"), { force: true });
    },
  };
}

/** An origin from configuration, or the build stops: a CSP built on a typo would block sign-in. */
function originOf(name: string, value: string): string {
  try {
    const url = new URL(value);
    if (url.protocol === "https:" || url.protocol === "http:") return url.origin;
  } catch {
    // Reported below.
  }
  throw new Error(`${name} must hold origins like https://id.example.com; got ${JSON.stringify(value)}`);
}

/**
 * The Console's Content-Security-Policy, as a meta tag in every build, so it
 * holds wherever the files are served. Scripts only from the Console's own
 * files; requests only to itself, the API and the sign-in provider's origins
 * (`VITE_ZUPFLOW_OIDC_ORIGINS`: its issuer, and its token endpoint's origin
 * when that differs). Inline styles are allowed because the UI's components
 * position overlays with `style` attributes. `frame-ancestors` has no effect
 * in a meta tag: whatever serves the Console must send it as a header (see
 * `FRAME_HEADERS`).
 */
function contentSecurityPolicy(env: Record<string, string>): string {
  const api = env.VITE_ZUPFLOW_API_URL?.trim();
  const providers = (env.VITE_ZUPFLOW_OIDC_ORIGINS ?? "")
    .split(",")
    .map((origin) => origin.trim())
    .filter((origin) => origin !== "")
    .map((origin) => originOf("VITE_ZUPFLOW_OIDC_ORIGINS", origin));
  const connect = new Set(["'self'", ...(api ? [originOf("VITE_ZUPFLOW_API_URL", api)] : []), ...providers]);
  return [
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data:",
    "font-src 'self' data:",
    `connect-src ${[...connect].join(" ")}`,
    "worker-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
  ].join("; ");
}

/** What only a response header can say: no page may frame the Console. */
const FRAME_HEADERS = {
  "Content-Security-Policy": "frame-ancestors 'none'",
  "X-Frame-Options": "DENY",
};

function withContentSecurityPolicy(policy: string): Plugin {
  return {
    name: "zupflow:content-security-policy",
    // Builds only: the dev server's hot reload injects inline scripts.
    apply: "build",
    transformIndexHtml: () => [
      { tag: "meta", attrs: { "http-equiv": "Content-Security-Policy", content: policy }, injectTo: "head-prepend" },
    ],
  };
}

export default defineConfig(({ command, mode }) => {
  // `.env` files and the process environment, as the app itself sees them.
  const env = loadEnv(mode, import.meta.dirname, "VITE_");
  const mock = mockIncluded(command, env.VITE_ZUPFLOW_MOCK);
  return {
    define: { __ZUPFLOW_MOCK__: JSON.stringify(mock) },
    plugins: [
      tanstackRouter({ target: "react", autoCodeSplitting: true, quoteStyle: "double" }),
      react(),
      tailwindcss(),
      withContentSecurityPolicy(contentSecurityPolicy(env)),
      ...(mock ? [] : [withoutMockWorker()]),
    ],
    resolve: {
      alias: {
        "@": path.resolve(import.meta.dirname, "src"),
      },
    },
    server: {
      port: Number(process.env.PORT ?? 5180),
      strictPort: true,
      headers: FRAME_HEADERS,
    },
    preview: {
      headers: FRAME_HEADERS,
    },
  };
});
