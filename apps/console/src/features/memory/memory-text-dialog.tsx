import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { characters, noteText } from "./labels";

/**
 * Writes a memory's text: a new one, or a change to one. For `notes` it
 * counts characters as the kind does, on the line the kind keeps; any kind's
 * own refusal of the text shows under the field, and other refusals above the
 * buttons.
 */
export function MemoryTextDialog({
  title,
  description,
  initial = "",
  open,
  onOpenChange,
  pending,
  /** The most characters the type's kind keeps, when the Console knows it (`notes`). */
  limit,
  /** The kind's refusal of the text, from the server. */
  textError,
  /** Any other refusal, as what happened and what to do. */
  refusal,
  actionLabel,
  onSave,
}: {
  title: string;
  description: React.ReactNode;
  initial?: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  pending: boolean;
  limit: number | null;
  textError: string | null;
  refusal: string | null;
  actionLabel: string;
  onSave: (text: string) => void;
}) {
  const [text, setText] = useState(initial);
  const [problem, setProblem] = useState<string | null>(null);
  // The server's refusal of the text shows until the text changes, and again after the next save.
  const [edited, setEdited] = useState(false);
  useEffect(() => {
    if (open) {
      setText(initial);
      setProblem(null);
      setEdited(false);
    }
  }, [open, initial]);
  const kept = limit === null ? text.trim() : noteText(text);
  const length = characters(kept);
  const error = problem ?? (edited ? null : textError);

  const save = () => {
    if (!kept) return setProblem("Write the memory.");
    if (limit !== null && length > limit) {
      return setProblem(
        `Use ${limit.toLocaleString("en")} characters or fewer. It has ${length.toLocaleString("en")}.`,
      );
    }
    setProblem(null);
    setEdited(false);
    onSave(text);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <form
          id="memory-text"
          noValidate
          className="min-w-0 space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            save();
          }}
        >
          <Label htmlFor="memory-text-field">Memory</Label>
          <Textarea
            id="memory-text-field"
            rows={4}
            // A long URL or token wraps instead of widening the dialog past the screen.
            className="max-w-full [overflow-wrap:anywhere]"
            value={text}
            autoFocus
            aria-invalid={error ? true : undefined}
            aria-describedby="memory-text-help"
            onChange={(e) => {
              setText(e.target.value);
              setProblem(null);
              setEdited(true);
            }}
            onKeyDown={(e) => {
              // An input method's Enter confirms what's being composed: it saves nothing.
              if (e.nativeEvent.isComposing) return;
              // One line: Enter saves, as a line break would become a space anyway.
              if (limit !== null && e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                save();
              }
            }}
          />
          <div id="memory-text-help" className="flex items-start justify-between gap-3 text-xs">
            {error ? (
              <p className="text-destructive">{error}</p>
            ) : (
              <p className="text-muted-foreground">
                {limit !== null ? "One line: line breaks become spaces. " : ""}
                Never save secrets, passwords or keys.
              </p>
            )}
            {limit !== null ? (
              <span className={cn("tabular shrink-0", length > limit ? "text-destructive" : "text-muted-foreground")}>
                {length.toLocaleString("en")} / {limit.toLocaleString("en")}
              </span>
            ) : null}
          </div>
          {refusal ? (
            <p role="alert" className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
              {refusal}
            </p>
          ) : null}
        </form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="memory-text" disabled={pending}>
            {pending ? <Spinner /> : null}
            {actionLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
