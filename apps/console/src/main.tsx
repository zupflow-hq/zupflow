import "./styles.css";
import "@/lib/router-types";
import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRouter, RouterProvider } from "@tanstack/react-router";
import { type Viewer, ZupflowApiError } from "@zupflow/sdk";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { session, startSession } from "@/features/auth/session";
import { setOrganizationSource } from "@/lib/api";
import { canonicalizeEnvironmentUrl, environmentRewrite } from "@/lib/environment-url";
import { qk } from "@/lib/query-keys";
import { ThemeProvider } from "@/lib/theme";
import { routeTree } from "./routeTree.gen";

const queryClient = new QueryClient({
  queryCache: new QueryCache({
    onError: (error) => {
      // An expired or missing session sends the person back to sign in, then
      // back here. (With `local` there's no session to lose.)
      if (error instanceof ZupflowApiError && error.status === 401 && session.canSignOut()) {
        const here = router.latestLocation.href;
        session.signOut();
        void router.navigate({ to: "/sign-in", search: { redirect: here.startsWith("/sign-in") ? undefined : here } });
      }
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 15_000,
      refetchOnWindowFocus: true,
      retry: (failures, error) => !(error instanceof ZupflowApiError && error.status < 500) && failures < 2,
    },
  },
});

canonicalizeEnvironmentUrl();

const router = createRouter({
  routeTree,
  context: { queryClient },
  defaultPreload: "intent",
  defaultPreloadStaleTime: 0,
  scrollRestoration: true,
  rewrite: environmentRewrite,
});

// Workspace requests name the organization of the page the router is on (the
// first path segment), read as each request is sent. Preloading another
// organization's link doesn't move the router, so it can't change this.
setOrganizationSource(() => {
  const slug = router.latestLocation.pathname.split("/")[1];
  const viewer = queryClient.getQueryData<Viewer>(qk.viewer);
  return viewer?.organizations.find((o) => o.slug === slug)?.id ?? null;
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

session.subscribe(() => {
  queryClient.clear();
});

async function start() {
  // Knows how to sign in (or that the API can't be reached) before the first route loads.
  await startSession();
  const root = document.getElementById("root");
  if (!root) throw new Error("Missing #root");
  createRoot(root).render(
    <StrictMode>
      <ThemeProvider>
        <QueryClientProvider client={queryClient}>
          <RouterProvider router={router} />
        </QueryClientProvider>
      </ThemeProvider>
    </StrictMode>,
  );
}

void start();
