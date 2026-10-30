import { InfoIcon } from "lucide-react";
import { StatusPill } from "@/components/app/status";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";

/**
 * A pill that says what it means when it's clicked, tapped or pressed: "Not
 * served", "After app results". A popover, not a tooltip, so a touch screen
 * reaches the explanation as a mouse and a keyboard do.
 */
export function Mark({
  tone,
  icon,
  label,
  explanation,
}: {
  tone: "warning" | "neutral";
  icon?: React.ReactNode;
  label: string;
  explanation: string;
}) {
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button type="button" className="rounded-full outline-none focus-visible:ring-2 focus-visible:ring-ring/50">
          <StatusPill tone={tone}>
            {icon}
            {label}
            <InfoIcon aria-hidden className="size-3 opacity-70" />
          </StatusPill>
        </button>
      </PopoverTrigger>
      <PopoverContent
        side="top"
        collisionPadding={16}
        aria-label={label}
        className="w-auto max-w-[min(18rem,var(--radix-popover-content-available-width))] p-3 text-xs leading-relaxed"
      >
        {explanation}
      </PopoverContent>
    </Popover>
  );
}
