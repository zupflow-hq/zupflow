import { useState } from "react";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";

/**
 * Confirmation for destructive actions. With `confirmText`, the person types
 * it before the action unlocks — reserved for actions that delete data.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  actionLabel,
  onConfirm,
  pending = false,
  confirmText,
  destructive = true,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: React.ReactNode;
  description: React.ReactNode;
  actionLabel: string;
  onConfirm: () => void;
  pending?: boolean;
  confirmText?: string;
  destructive?: boolean;
}) {
  const [typed, setTyped] = useState("");
  // Each opening starts empty, however the last one ended: Cancel, or the caller closing it once the action is done.
  const [shownOpen, setShownOpen] = useState(open);
  if (open !== shownOpen) {
    setShownOpen(open);
    setTyped("");
  }
  const locked = confirmText !== undefined && typed.trim() !== confirmText;
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="space-y-2 text-sm text-muted-foreground">{description}</div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        {confirmText !== undefined ? (
          <div className="space-y-2">
            {/* A sentence, so a long name wraps in its line instead of squeezing the words beside it. */}
            <Label htmlFor="confirm-text" className="block leading-relaxed font-normal text-muted-foreground">
              Type{" "}
              <code className="rounded bg-muted box-decoration-clone px-1 py-0.5 text-foreground [overflow-wrap:anywhere]">
                {confirmText}
              </code>{" "}
              to confirm
            </Label>
            <Input id="confirm-text" value={typed} autoComplete="off" onChange={(e) => setTyped(e.target.value)} />
          </div>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>Cancel</AlertDialogCancel>
          <Button variant={destructive ? "destructive" : "default"} disabled={locked || pending} onClick={onConfirm}>
            {pending ? <Spinner /> : null}
            {actionLabel}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
