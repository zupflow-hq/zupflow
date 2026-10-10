import { createFileRoute, redirect } from "@tanstack/react-router";
import { ArrowRightIcon, RotateCwIcon, ServerCrashIcon } from "lucide-react";
import { lazy, Suspense, useState } from "react";
import { z } from "zod";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import type { OidcControls, UnavailableControls } from "@/features/auth/adapter";
import { session } from "@/features/auth/session";
import { providerName, SignInHeading, SignInLayout, SignInSkeleton } from "@/features/auth/sign-in-layout";
import { safeReturnPath, signInFailureMessage } from "@/lib/oidc";

export const Route = createFileRoute("/sign-in")({
  validateSearch: z.object({ redirect: z.string().optional() }),
  beforeLoad: () => {
    // With `local` nobody signs in: the server is the built-in owner.
    if (session.isSignedIn()) throw redirect({ to: "/" });
  },
  component: SignInPage,
});

/** The mock's simulated sign-in, in builds that have the mock only. */
const MockSignIn = __ZUPFLOW_MOCK__ ? lazy(() => import("@/features/auth/mock-sign-in")) : null;

function SignInPage() {
  const { redirect: target } = Route.useSearch();
  const controls = session.controls();
  switch (controls.mode) {
    case "mock":
      return MockSignIn ? (
        <Suspense fallback={<SignInSkeleton />}>
          <MockSignIn controls={controls} target={target} />
        </Suspense>
      ) : null;
    case "oidc":
      return <OidcSignIn controls={controls} target={target} />;
    case "unavailable":
      return <ApiUnavailable controls={controls} />;
    case "local":
      // Never shown: `local` is always signed in.
      return null;
  }
}

function OidcSignIn({ controls, target }: { controls: OidcControls; target: string | undefined }) {
  const [state, setState] = useState<{ pending: boolean; error: string | null }>({ pending: false, error: null });
  const provider = providerName(controls.issuer);
  return (
    <SignInLayout footer={`Sign-in is handled by ${provider}.`}>
      <SignInHeading
        title="Sign in to the Console"
        description="Manage workspaces, apps and agents for your product."
      />
      <div className="mt-8 space-y-3">
        <Button
          className="h-11 w-full justify-between"
          disabled={state.pending}
          onClick={async () => {
            setState({ pending: true, error: null });
            try {
              await controls.signIn(safeReturnPath(target));
            } catch (error) {
              setState({ pending: false, error: signInFailureMessage(error) });
            }
          }}
        >
          Continue with single sign-on
          {state.pending ? <Spinner /> : <ArrowRightIcon />}
        </Button>
        <p className="text-center text-xs text-muted-foreground">
          You'll sign in at <span className="font-medium break-all text-foreground">{provider}</span>
        </p>
        {state.error ? (
          <p role="alert" className="text-sm text-destructive">
            {state.error}
          </p>
        ) : null}
      </div>
    </SignInLayout>
  );
}

function ApiUnavailable({ controls }: { controls: UnavailableControls }) {
  return (
    <SignInLayout>
      <ServerCrashIcon className="mb-4 size-6 text-muted-foreground" aria-hidden />
      <SignInHeading title="Can't reach the Zupflow API" description={controls.reason} />
      <p className="mt-6 text-xs text-muted-foreground">The Console looked for it at</p>
      <code className="mt-1 block rounded-md border bg-muted/50 px-3 py-2 font-mono text-xs break-all">
        {controls.apiUrl}
      </code>
      <Button className="mt-6 h-11 w-full" variant="outline" onClick={() => window.location.reload()}>
        <RotateCwIcon />
        Try again
      </Button>
    </SignInLayout>
  );
}
