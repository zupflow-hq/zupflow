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
  const locked = confirmText !== undefined && typed.trim() !== confirmText;
  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (!next) setTyped("");
        onOpenChange(next);
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="space-y-2 text-sm text-muted-foreground">{description}</div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        {confirmText !== undefined ? (
          <div className="space-y-2">
            <Label htmlFor="confirm-text" className="font-normal text-muted-foreground">
              Type <code className="rounded bg-muted px-1 py-0.5 text-foreground">{confirmText}</code> to confirm
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
