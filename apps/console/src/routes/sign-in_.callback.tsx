import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { RotateCwIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { session } from "@/features/auth/session";
import { SignInLayout, SignInSkeleton } from "@/features/auth/sign-in-layout";
import { safeReturnPath } from "@/lib/oidc";

/**
 * Where the identity provider sends the browser back (`oidc` only). The
 * Console's address here, `{origin}/sign-in/callback`, is the redirect URI
 * registered with the provider.
 */
export const Route = createFileRoute("/sign-in_/callback")({
  component: SignInCallbackPage,
});

function SignInCallbackPage() {
  const navigate = useNavigate();
  const controls = session.controls();
  const [failure, setFailure] = useState<{ message: string; returnTo: string } | null>(
    controls.mode === "oidc"
      ? null
      : { message: "There's no sign-in to finish here. Sign in from the start.", returnTo: "/" },
  );

  useEffect(() => {
    if (controls.mode !== "oidc") return;
    let current = true;
    void controls.completeSignIn(new URL(window.location.href)).then((completion) => {
      if (!current) return;
      if (completion.ok) void navigate({ href: safeReturnPath(completion.returnTo), replace: true });
      else setFailure({ message: completion.message, returnTo: completion.returnTo });
    });
    return () => {
      current = false;
    };
  }, [controls, navigate]);

  if (failure === null) {
    return <SignInSkeleton label="Signing you in" />;
  }

  return (
    <SignInLayout>
      <h1 className="text-2xl font-semibold tracking-[-0.02em]">Sign-in didn't finish</h1>
      <p role="alert" className="mt-1.5 text-sm text-muted-foreground">
        {failure.message}
      </p>
      <div className="mt-8 space-y-3">
        {controls.mode === "oidc" ? (
          <Button className="h-11 w-full" onClick={() => void controls.signIn(failure.returnTo)}>
            <RotateCwIcon />
            Sign in again
          </Button>
        ) : null}
        <Button asChild variant="ghost" className="w-full text-muted-foreground">
          <Link to="/sign-in">Back to sign-in</Link>
        </Button>
      </div>
    </SignInLayout>
  );
}
