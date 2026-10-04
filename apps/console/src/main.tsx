import "./styles.css";
import "@/lib/router-types";
import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRouter, RouterProvider } from "@tanstack/react-router";
import { ZupflowApiError } from "@zupflow/sdk";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { session } from "@/features/auth/session";
import { config } from "@/lib/env";
import { canonicalizeEnvironmentUrl, environmentRewrite } from "@/lib/environment-url";
import { ThemeProvider } from "@/lib/theme";
import { routeTree } from "./routeTree.gen";

const queryClient = new QueryClient({
  queryCache: new QueryCache({
    onError: (error) => {
      // An expired or missing session sends the person back to sign in.
      if (error instanceof ZupflowApiError && error.status === 401 && session.isSignedIn()) {
        session.signOut();
        void router.navigate({ to: "/sign-in" });
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

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

session.subscribe(() => {
  queryClient.clear();
});

async function start() {
  if (config.mockApi) {
    const { startMockApi } = await import("@/mocks/browser");
    await startMockApi(config.apiUrl);
  }
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
