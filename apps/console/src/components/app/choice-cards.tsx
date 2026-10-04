import { RadioGroup } from "radix-ui";
import { cn } from "@/lib/utils";

export interface Choice<T extends string> {
  value: T;
  content: React.ReactNode;
  disabled?: boolean;
  /** Extra classes, typically `data-[state=checked]:` colors for this choice. */
  className?: string;
}

/**
 * A radio group whose options are cards with rich content. It is the Radix
 * radio primitive behind shadcn's RadioGroup, so arrow keys, focus and form
 * semantics come for free; only the item rendering differs.
 */
export function ChoiceCards<T extends string>({
  value,
  onValueChange,
  options,
  label,
  className,
}: {
  value: T | undefined;
  onValueChange: (value: T) => void;
  options: Choice<T>[];
  label: string;
  className?: string;
}) {
  return (
    <RadioGroup.Root
      value={value ?? ""}
      onValueChange={(v) => onValueChange(v as T)}
      aria-label={label}
      className={cn("grid gap-2", className)}
    >
      {options.map((o) => (
        <RadioGroup.Item
          key={o.value}
          value={o.value}
          disabled={o.disabled}
          className={cn(
            "flex flex-col items-start gap-1.5 rounded-lg border p-3 text-left transition-colors outline-none hover:bg-accent/50 focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50",
            "data-[state=checked]:border-foreground/60 data-[state=checked]:bg-accent/60",
            o.className,
          )}
        >
          {o.content}
        </RadioGroup.Item>
      ))}
    </RadioGroup.Root>
  );
}
