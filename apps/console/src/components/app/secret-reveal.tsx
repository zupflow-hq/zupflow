import { CheckIcon, CopyIcon, TriangleAlertIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useCopy } from "./misc";

/**
 * Shows a newly created secret exactly once. It lives only in this
 * component's props; closing the dialog drops it.
 */
export function SecretRevealDialog({
  title,
  description,
  secret,
  onClose,
}: {
  title: string;
  description: React.ReactNode;
  secret: string | null;
  onClose: () => void;
}) {
  const { copied, copy } = useCopy(2500);
  return (
    <Dialog open={secret !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-lg" onInteractOutside={(e) => e.preventDefault()}>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <div className="flex items-stretch gap-2">
          <code className="flex min-w-0 flex-1 items-center rounded-md border bg-muted/50 px-3 py-2 font-mono text-[12.5px] break-all select-all">
            {secret}
          </code>
          <Button variant="outline" className="h-auto" onClick={() => secret && copy(secret)}>
            {copied ? <CheckIcon className="text-success" /> : <CopyIcon />}
            {copied ? "Copied" : "Copy"}
          </Button>
        </div>
        <p className="flex gap-2 rounded-lg bg-warning-soft/70 p-3 text-sm">
          <TriangleAlertIcon className="mt-0.5 size-4 shrink-0 text-warning" />
          Store it in your secret manager now. Zupflow keeps only a hash and can't show it again.
        </p>
        <DialogFooter>
          <Button onClick={onClose}>I've stored it</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
