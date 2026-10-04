import { Link } from "@tanstack/react-router";
import { ZupflowMark } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";

export function NotFound() {
  return (
    <main className="grid min-h-svh place-items-center px-4">
      <div className="flex max-w-sm flex-col items-center gap-4 text-center">
        <ZupflowMark className="size-10" />
        <div className="space-y-1">
          <h1 className="text-xl font-semibold">Page not found</h1>
          <p className="text-sm text-muted-foreground">
            The link may be out of date, or the resource belongs to an organization you're not a member of.
          </p>
        </div>
        <Button asChild variant="outline">
          <Link to="/">Go to the Console</Link>
        </Button>
      </div>
    </main>
  );
}
